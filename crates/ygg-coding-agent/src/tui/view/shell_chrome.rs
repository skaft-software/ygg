use std::time::Instant;

use sexy_tui_rs::visible_width;

use super::input_overlays::{render_input_suggestions, render_pending_steering};
use super::panel_render::render_panel_with_limit;
use super::terminal_text::sanitize_for_terminal;
use super::{fit_line, semantic_separator, wrap_hanging, ShellState};

#[derive(Clone)]
pub(super) struct ShellChrome {
    pub(super) header: Vec<String>,
    pub(super) subagents: Vec<String>,
    pub(super) extension_above: Vec<String>,
    pub(super) composer: Vec<String>,
    pub(super) extension_below: Vec<String>,
    pub(super) panel: Vec<String>,
    pub(super) pending: Vec<String>,
    pub(super) suggestions: Vec<String>,
    pub(super) error: Vec<String>,
    pub(super) transcript_rows: usize,
}

pub(super) fn responsive_identity(state: &ShellState, width: u16) -> String {
    let wordmark = state.theme.bold(
        &state
            .theme
            .fg("model_accent", state.theme.glyph("wordmark")),
    );
    if state.model.is_empty() {
        return fit_line(&wordmark, width);
    }
    let provider = sanitize_for_terminal(&state.provider);
    let model_name = if state.model_display.is_empty() {
        &state.model
    } else {
        &state.model_display
    };
    let model = state
        .theme
        .fg("model_accent", &sanitize_for_terminal(model_name));
    let separator = semantic_separator(&state.theme);
    let reasoning = (!state.reasoning.is_empty() && state.reasoning != "off")
        .then(|| format!("{separator}{}", sanitize_for_terminal(&state.reasoning)));
    let provider_model = format!("{provider} / {model}");
    let right = format!("{provider_model}{}", reasoning.clone().unwrap_or_default());
    let wide_width = visible_width(&wordmark) + visible_width(&right) + 4;
    if usize::from(width) >= 72 && wide_width <= usize::from(width) {
        let gap =
            usize::from(width).saturating_sub(visible_width(&wordmark) + visible_width(&right));
        return format!("{wordmark}{}{right}", " ".repeat(gap));
    }

    let compact = format!(
        "{wordmark}{separator}{}/{}{}",
        provider,
        model,
        reasoning.unwrap_or_default()
    );
    if visible_width(&compact) <= usize::from(width) {
        return compact;
    }
    let model_only = format!("{wordmark}{separator}{model}");
    if visible_width(&model_only) <= usize::from(width) {
        return model_only;
    }
    fit_line(&wordmark, width)
}

fn render_shell_header(state: &ShellState, width: u16) -> Vec<String> {
    let layout = state.theme.layout_for_width(width);
    if layout.show_header {
        vec![responsive_identity(state, width)]
    } else {
        Vec::new()
    }
}

fn render_extension_line(state: &ShellState, text: &str, role: Option<&str>, width: u16) -> String {
    let text = sanitize_for_terminal(text);
    let styled = match role {
        Some("extension.pi.muted") => state.theme.fg("muted", &text),
        Some("extension.pi.accent") => state.theme.fg("model_accent", &text),
        Some("extension.pi.warning") => state.theme.fg("warning", &text),
        Some("extension.pi.error") => state.theme.fg("error", &text),
        Some("extension.pi.status") | None => state.theme.fg("foreground", &text),
        // Transport validation admits only the roles above. Retain this
        // neutral fallback at the rendering boundary as defense in depth.
        Some(_) => state.theme.fg("foreground", &text),
    };
    fit_line(&styled, width)
}

fn render_extension_ui(state: &ShellState, width: u16) -> (Vec<String>, Vec<String>) {
    let mut above = state
        .extension_ui
        .above_editor
        .iter()
        .map(|line| render_extension_line(state, &line.text, line.style_role.as_deref(), width))
        .collect::<Vec<_>>();
    above.extend(
        state.extension_ui.statuses.iter().map(|line| {
            render_extension_line(state, &line.text, line.style_role.as_deref(), width)
        }),
    );
    if state.run.is_active() {
        if let Some(working) = &state.extension_ui.working {
            if working.visible != Some(false) {
                let message = working.message.as_deref().or_else(|| {
                    working.frames.as_ref().and_then(|frames| {
                        (!frames.is_empty())
                            .then(|| frames[state.event_spinner_frame % frames.len()].as_str())
                    })
                });
                if let Some(message) = message {
                    above.push(render_extension_line(
                        state,
                        message,
                        Some("extension.pi.accent"),
                        width,
                    ));
                }
            }
        }
        if let Some(label) = state.extension_ui.hidden_thinking_label.as_deref() {
            above.push(render_extension_line(
                state,
                &format!("thinking: {label}"),
                Some("extension.pi.muted"),
                width,
            ));
        }
    }
    let below = state
        .extension_ui
        .below_editor
        .iter()
        .map(|line| render_extension_line(state, &line.text, line.style_role.as_deref(), width))
        .collect();
    (above, below)
}

fn render_subagent_activity(state: &ShellState, width: u16) -> Vec<String> {
    // Delegated workers now render as persistent transcript tool blocks. Keep
    // this compatibility path empty so older shell-chrome callers cannot
    // duplicate the event below the transcript.
    if state.subagent_activity_block.is_some() {
        return Vec::new();
    }
    let Some(view) = state.subagent_activity.as_ref() else {
        return Vec::new();
    };
    let status_label = view.status_label.clone();
    let mut lines = vec![fit_line(
        &state.theme.bold(
            &state
                .theme
                .fg("model_accent", &sanitize_for_terminal(&status_label)),
        ),
        width,
    )];
    let unicode = state.theme.unicode();
    // The host bounds the roster, so keep every worker visible even while
    // ordinary tool disclosure is collapsed.
    if !view.telemetry.is_empty() {
        let children = &view.telemetry;
        for (index, child) in children.iter().rev().enumerate() {
            let last = index + 1 == children.len();
            let branch = match (unicode, last) {
                (true, true) => "└",
                (true, false) => "├",
                (false, true) => "\\",
                (false, false) => "+",
            };
            let continuation = if last {
                "  "
            } else if unicode {
                "│ "
            } else {
                "| "
            };
            let phase = child
                .current_tool
                .as_deref()
                .or_else(|| (!child.phase.is_empty()).then_some(child.phase.as_str()));
            let summary = match phase {
                Some(phase) if !matches!(child.state.as_str(), "completed" | "failed") => {
                    format!("{} · {}", child.task_name, phase.replace('_', " "))
                }
                _ => child.task_name.clone(),
            };
            let input = child
                .input_tokens
                .saturating_add(child.cache_read_tokens)
                .saturating_add(child.cache_write_tokens);
            let calls = if child.tool_use_count == 1 {
                "1 Tool Call".to_owned()
            } else {
                format!("{} Tool Calls", child.tool_use_count)
            };
            let mut telemetry = if unicode {
                format!(
                    "{calls} • ↑{} ↓{}",
                    crate::tui::composer_surface::compact_token_count(input),
                    crate::tui::composer_surface::compact_token_count(child.output_tokens),
                )
            } else {
                format!(
                    "{calls} - in {} out {}",
                    crate::tui::composer_surface::compact_token_count(input),
                    crate::tui::composer_surface::compact_token_count(child.output_tokens),
                )
            };
            if let Some(cost) = child.cost_microdollars {
                telemetry.push_str(if unicode { " • " } else { " - " });
                telemetry.push_str(&crate::tui::composer_surface::format_microdollars(cost));
            }
            lines.push(fit_line(
                &format!(
                    "{} {}",
                    state.theme.fg("muted", branch),
                    state
                        .theme
                        .fg("foreground", &sanitize_for_terminal(&summary))
                ),
                width,
            ));
            let detail = if let Some(reason) = child.failure_reason.as_deref() {
                format!("Failed: {}", sanitize_for_terminal(reason))
            } else if child.state == "completed" {
                format!("Done · {telemetry}")
            } else if let Some(tool) = child.current_tool.as_deref() {
                sanitize_for_terminal(tool)
            } else {
                telemetry.clone()
            };
            lines.push(fit_line(
                &format!(
                    "{}{} {}",
                    state.theme.fg("muted", continuation),
                    state.theme.fg("muted", if unicode { "└" } else { "|_" }),
                    state.theme.fg("muted", &detail),
                ),
                width,
            ));
            if child.failure_reason.is_none() && child.state != "completed" {
                lines.push(fit_line(
                    &format!(
                        "{}{} {}",
                        state.theme.fg("muted", continuation),
                        state.theme.fg("muted", if unicode { "·" } else { "-" }),
                        state.theme.fg("muted", &telemetry),
                    ),
                    width,
                ));
            }
        }
        if let Some(reason) = view.failure_reason.as_deref() {
            lines.push(fit_line(
                &format!(
                    "{} {}",
                    state.theme.fg("error", if unicode { "└" } else { "|_" }),
                    state.theme.fg(
                        "error",
                        &format!("Failed: {}", sanitize_for_terminal(reason)),
                    )
                ),
                width,
            ));
        }
    } else if let Some(reason) = view.failure_reason.as_deref() {
        lines.push(fit_line(
            &format!(
                "{} {}",
                state.theme.fg("error", if unicode { "└" } else { "|_" }),
                state.theme.fg(
                    "error",
                    &format!("Failed: {}", sanitize_for_terminal(reason))
                )
            ),
            width,
        ));
    } else {
        let activities = &view.activities;
        for (index, activity) in activities.iter().rev().enumerate() {
            let last = index + 1 == activities.len();
            let branch = match (unicode, last) {
                (true, true) => "└",
                (true, false) => "├",
                (false, true) => "\\",
                (false, false) => "+",
            };
            let continuation = if last {
                "  "
            } else if unicode {
                "│ "
            } else {
                "| "
            };
            lines.push(fit_line(
                &format!(
                    "{} {}",
                    state.theme.fg("muted", branch),
                    state
                        .theme
                        .fg("foreground", &sanitize_for_terminal(&activity.summary))
                ),
                width,
            ));
            if let Some(metrics) = activity.metrics {
                let input = metrics
                    .input_tokens
                    .saturating_add(metrics.cache_read_tokens)
                    .saturating_add(metrics.cache_write_tokens);
                let calls = if metrics.tool_calls == 1 {
                    "1 Tool Call".to_owned()
                } else {
                    format!("{} Tool Calls", metrics.tool_calls)
                };
                let mut telemetry = if unicode {
                    format!(
                        "{calls} • ↑{} ↓{}",
                        crate::tui::composer_surface::compact_token_count(input),
                        crate::tui::composer_surface::compact_token_count(metrics.output_tokens),
                    )
                } else {
                    format!(
                        "{calls} - in {} out {}",
                        crate::tui::composer_surface::compact_token_count(input),
                        crate::tui::composer_surface::compact_token_count(metrics.output_tokens),
                    )
                };
                if let Some(cost) = metrics.cost_microdollars {
                    telemetry.push_str(if unicode { " • " } else { " - " });
                    telemetry.push_str(&crate::tui::composer_surface::format_microdollars(cost));
                }
                lines.push(fit_line(
                    &format!(
                        "{}{} {}",
                        state.theme.fg("muted", continuation),
                        state.theme.fg("muted", if unicode { "└" } else { "|_" }),
                        state.theme.fg("muted", &telemetry),
                    ),
                    width,
                ));
            }
        }
    }
    lines
}

pub(super) fn shell_chrome(state: &ShellState, width: u16, now: Instant) -> ShellChrome {
    let rows = usize::from(state.size.1.max(5));
    let header = render_shell_header(state, width);
    let mut error = state
        .error
        .as_ref()
        .map(|error| {
            let marker = state.theme.fg("error", state.theme.glyph("error"));
            let first_prefix = format!("  {marker} ");
            let continuation = " ".repeat(visible_width(&first_prefix));
            let mut rendered = Vec::new();
            for (index, source) in sanitize_for_terminal(error).split('\n').enumerate() {
                if source.is_empty() {
                    rendered.push(String::new());
                    continue;
                }
                let prefix = if index == 0 {
                    first_prefix.as_str()
                } else {
                    continuation.as_str()
                };
                rendered.extend(wrap_hanging(
                    &state.theme.fg("foreground", source),
                    prefix,
                    &continuation,
                    width,
                ));
            }
            rendered
        })
        .unwrap_or_default();

    // Render the integrated composer surface with its ordinary status row.
    // Autocomplete can claim that row below once we know it has real matches.
    let footer_visible = crate::tui::composer_surface::status_footer_visible(state, width);
    let mut composer = crate::tui::composer_surface::render_composer_surface(state, width, now);
    let mut subagents = if state.panel.is_none() {
        render_subagent_activity(state, width)
    } else {
        Vec::new()
    };
    let (mut extension_above, mut extension_below) = if state.panel.is_none() {
        render_extension_ui(state, width)
    } else {
        (Vec::new(), Vec::new())
    };
    let subagent_limit = rows.saturating_sub(header.len() + error.len() + composer.len() + 1);
    // The roster is bounded by the host's eight-concurrent-children cap, but
    // each child renders several rows, so only the viewport bounds how much
    // of the strip shows.
    subagents.truncate(subagent_limit);
    let extension_limit = rows.saturating_sub(
        header
            .len()
            .saturating_add(error.len())
            .saturating_add(subagents.len())
            .saturating_add(composer.len())
            .saturating_add(1),
    );
    extension_above.truncate(extension_limit);
    extension_below.truncate(extension_limit.saturating_sub(extension_above.len()));
    if state.panel.is_some() {
        // The focused picker must retain at least its filter row and cursor,
        // even when a tiny terminal also has a wrapped error message.
        let error_limit = rows.saturating_sub(
            composer
                .len()
                .saturating_add(header.len())
                .saturating_add(1),
        );
        error.truncate(error_limit);
    }
    let mut remaining = rows.saturating_sub(
        header
            .len()
            .saturating_add(error.len())
            .saturating_add(subagents.len())
            .saturating_add(extension_above.len())
            .saturating_add(composer.len())
            .saturating_add(extension_below.len()),
    );

    let panel = render_panel_with_limit(state, width, remaining);
    remaining = remaining.saturating_sub(panel.len());

    // Let autocomplete reuse the status row, including in a short terminal
    // where that reclaimed row is what makes a choice plus its hint fit.
    let suggestion_limit = remaining
        .saturating_add(if footer_visible { 1 } else { 0 })
        .min(10);
    let suggestions = render_input_suggestions(state, width, suggestion_limit);
    if footer_visible && !suggestions.is_empty() {
        composer.pop();
        remaining = remaining.saturating_add(1);
    }
    remaining = remaining.saturating_sub(suggestions.len());

    // Pending steering is a compact preview, never a second transcript.
    let pending_limit = remaining.min(crate::tui::layout::MAX_STEERING_PREVIEW_ROWS);
    let pending = render_pending_steering(state, width, pending_limit);
    remaining = remaining.saturating_sub(pending.len());

    ShellChrome {
        header,
        subagents,
        extension_above,
        composer,
        extension_below,
        panel,
        pending,
        suggestions,
        error,
        transcript_rows: remaining,
    }
}

pub(super) fn append_viewport_chrome(lines: &mut Vec<String>, chrome: ShellChrome) {
    // Application-owned mode renders exactly one terminal viewport. The
    // terminal-owned mode uses `append_chrome` below so committed transcript
    // rows can enter native scrollback instead of being sliced away here.
    lines.truncate(chrome.transcript_rows);
    lines.resize(chrome.transcript_rows, String::new());
    lines.extend(chrome.header);
    lines.extend(chrome.error);
    lines.extend(chrome.pending);
    lines.extend(chrome.panel);
    lines.extend(chrome.subagents);
    lines.extend(chrome.extension_above);
    lines.extend(chrome.composer);
    lines.extend(chrome.suggestions);
    lines.extend(chrome.extension_below);
}

pub(super) fn append_chrome(
    lines: &mut Vec<String>,
    chrome: ShellChrome,
    stable_prefix_rows: usize,
) {
    // The default terminal-owned mode follows logical content height. Padding
    // a short frame to terminal height would pin the composer to the bottom and
    // create a large dead zone below the transcript. Once the frame naturally
    // grows past the viewport, sexy-tui moves committed rows into native
    // scrollback.
    // `lines` may be only a lazy suffix, so its retained prefix still decides
    // whether the transcript owns the single breathing row before chrome.
    let complete_transcript_rows = stable_prefix_rows.saturating_add(lines.len());
    if complete_transcript_rows > 0 {
        lines.push(String::new());
    }
    lines.extend(chrome.header);
    lines.extend(chrome.error);
    lines.extend(chrome.pending);
    lines.extend(chrome.panel);
    lines.extend(chrome.subagents);
    lines.extend(chrome.extension_above);
    lines.extend(chrome.composer);
    // Keep autocomplete adjacent to the composer in terminal-owned mode as
    // well as in the application-owned viewport above.
    lines.extend(chrome.suggestions);
    lines.extend(chrome.extension_below);
}

pub(super) fn shell_chrome_rows(chrome: &ShellChrome) -> usize {
    chrome
        .header
        .len()
        .saturating_add(chrome.subagents.len())
        .saturating_add(chrome.extension_above.len())
        .saturating_add(chrome.error.len())
        .saturating_add(chrome.pending.len())
        .saturating_add(chrome.suggestions.len())
        .saturating_add(chrome.panel.len())
        .saturating_add(chrome.composer.len())
        .saturating_add(chrome.extension_below.len())
}
