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
    pub(super) composer: Vec<String>,
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

fn render_subagent_activity(state: &ShellState, width: u16) -> Vec<String> {
    let Some(view) = state.subagent_activity.as_ref() else {
        return Vec::new();
    };
    // When the strip is expanded (ctrl+o), the hint in the label flips so it
    // keeps telling the truth about what ctrl+o will do next.
    let status_label = if state.subagent_activity_expanded {
        view.status_label.replace("ctrl+o to expand", "ctrl+o to collapse")
    } else {
        view.status_label.clone()
    };
    let mut lines = vec![fit_line(
        &state.theme.bold(
            &state
                .theme
                .fg("model_accent", &sanitize_for_terminal(&status_label)),
        ),
        width,
    )];
    let unicode = state.theme.unicode();
    // Collapsed: the two most recent children. Expanded: the five most
    // recent, so a large team cannot push the whole frame past the viewport
    // (the full roster stays one /subagents away).
    let limit = if state.subagent_activity_expanded { 5 } else { 2 };
    if !view.telemetry.is_empty() {
        let children = view.telemetry.iter().rev().take(limit).collect::<Vec<_>>();
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
        let activities = view.activities
            .iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>();
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
    let subagent_limit = rows.saturating_sub(header.len() + error.len() + composer.len() + 1);
    subagents.truncate(subagent_limit.min(8));
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
    let mut remaining =
        rows.saturating_sub(header.len() + error.len() + subagents.len() + composer.len());

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

    // Queued steering may expand enough to show wrapped prompts, but remains
    // bounded to roughly one quarter of the viewport so transcript activity
    // stays visible on constrained terminals.
    let pending_limit = remaining.min((rows / 4).clamp(4, 8));
    let pending = render_pending_steering(state, width, pending_limit);
    remaining = remaining.saturating_sub(pending.len());

    ShellChrome {
        header,
        subagents,
        composer,
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
    lines.extend(chrome.composer);
    // Input discovery expands downward from the composer and temporarily
    // occupies the status row, keeping model/token telemetry from competing
    // with the active choices.
    lines.extend(chrome.suggestions);
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
    lines.extend(chrome.composer);
    // Keep autocomplete adjacent to the composer in terminal-owned mode as
    // well as in the application-owned viewport above.
    lines.extend(chrome.suggestions);
}

pub(super) fn shell_chrome_rows(chrome: &ShellChrome) -> usize {
    chrome
        .header
        .len()
        .saturating_add(chrome.subagents.len())
        .saturating_add(chrome.error.len())
        .saturating_add(chrome.pending.len())
        .saturating_add(chrome.suggestions.len())
        .saturating_add(chrome.panel.len())
        .saturating_add(chrome.composer.len())
}
