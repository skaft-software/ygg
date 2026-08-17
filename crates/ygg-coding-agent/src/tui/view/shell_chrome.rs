use std::time::Instant;

use sexy_tui_rs::visible_width;

use super::input_overlays::{render_input_suggestions, render_pending_steering};
use super::panel_render::render_panel_with_limit;
use super::terminal_text::sanitize_for_terminal;
use super::{fit_line, semantic_separator, wrap_hanging, ShellState};

#[derive(Clone)]
pub(super) struct ShellChrome {
    pub(super) header: Vec<String>,
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
    let mut lines = Vec::with_capacity(2);
    if layout.show_header {
        lines.push(responsive_identity(state, width));
    }
    if let Some((text, role)) = state
        .extension_header
        .as_ref()
        .filter(|(text, _)| !text.trim().is_empty())
    {
        let role = role.as_deref().unwrap_or("extension.header");
        let inset = usize::from(layout.transcript_inset).min(usize::from(width));
        let contribution = state
            .theme
            .apply_semantic_role(role, &sanitize_for_terminal(text));
        lines.push(fit_line(
            &format!("{}{contribution}", " ".repeat(inset)),
            width,
        ));
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
    let mut remaining = rows.saturating_sub(header.len() + error.len() + composer.len());

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
    lines.extend(chrome.composer);
    // Keep autocomplete adjacent to the composer in terminal-owned mode as
    // well as in the application-owned viewport above.
    lines.extend(chrome.suggestions);
}

pub(super) fn shell_chrome_rows(chrome: &ShellChrome) -> usize {
    chrome
        .header
        .len()
        .saturating_add(chrome.error.len())
        .saturating_add(chrome.pending.len())
        .saturating_add(chrome.suggestions.len())
        .saturating_add(chrome.panel.len())
        .saturating_add(chrome.composer.len())
}
