//! Composer-adjacent slash, mention, and queued-steering overlays.

use sexy_tui_rs::visible_width;

use super::{fit_line, semantic_separator, ShellState};
use crate::commands;
use crate::tui::composer;

#[derive(Clone, Debug)]
pub(super) struct InputSlashSuggestion {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) argument_hint: Option<String>,
    pub(super) accepts_argument: bool,
}

pub(super) fn input_slash_suggestions(state: &ShellState) -> Vec<InputSlashSuggestion> {
    let Some(query) = state.editor.strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) || query.contains('\n') {
        return Vec::new();
    }
    let mut suggestions = commands::slash_suggestions(&state.editor)
        .into_iter()
        .map(|command| InputSlashSuggestion {
            name: command.name.to_owned(),
            description: command.description.to_owned(),
            argument_hint: None,
            accepts_argument: command.accepts_argument,
        })
        .collect::<Vec<_>>();
    for template in state
        .prompt_templates
        .iter()
        .filter(|template| template.name.starts_with(query))
    {
        if suggestions
            .iter()
            .any(|suggestion| suggestion.name == template.name)
        {
            continue;
        }
        suggestions.push(InputSlashSuggestion {
            name: template.name.clone(),
            description: format!("prompt · {}", template.description),
            argument_hint: template.argument_hint.clone(),
            accepts_argument: true,
        });
    }
    for (name, description) in state
        .extension_commands
        .iter()
        .filter(|(name, _)| name.starts_with(query))
    {
        if suggestions
            .iter()
            .any(|suggestion| suggestion.name == *name)
        {
            continue;
        }
        suggestions.push(InputSlashSuggestion {
            name: name.clone(),
            description: format!("extension · {description}"),
            argument_hint: None,
            accepts_argument: true,
        });
    }
    suggestions
}

pub(super) fn render_slash_suggestions(
    state: &ShellState,
    width: u16,
    max_rows: usize,
) -> Vec<String> {
    if state.slash_popup_dismissed || max_rows < 2 {
        return Vec::new();
    }
    let suggestions = input_slash_suggestions(state);
    if suggestions.is_empty() {
        return Vec::new();
    }

    let item_rows = max_rows.saturating_sub(1).max(1);
    let selected = state
        .slash_selection
        .min(suggestions.len().saturating_sub(1));
    let max_start = suggestions.len().saturating_sub(item_rows);
    let mut start = state.slash_scroll.min(max_start);
    if selected < start {
        start = selected;
    } else if selected >= start.saturating_add(item_rows) {
        start = selected + 1 - item_rows;
    }
    start = start.min(max_start);
    let end = start.saturating_add(item_rows).min(suggestions.len());

    let heading = if suggestions.len() > item_rows {
        format!("  commands  {}–{}/{}", start + 1, end, suggestions.len())
    } else {
        "  commands".to_owned()
    };
    let mut lines = vec![state.theme.fg("muted", &fit_line(&heading, width))];
    let marker = state.theme.glyph("prompt");
    let label_width = suggestions[start..end]
        .iter()
        .map(|command| {
            visible_width(&format!(
                "/{}{}",
                command.name,
                command
                    .argument_hint
                    .as_deref()
                    .map(|hint| format!(" {hint}"))
                    .unwrap_or_default()
            ))
        })
        .max()
        .unwrap_or(1)
        .min(30)
        .min(usize::from(width).saturating_sub(6).max(1));
    for (index, command) in suggestions[start..end].iter().enumerate() {
        let absolute = start + index;
        let selected_row = absolute == selected;
        let prefix = if selected_row { marker } else { " " };
        let raw_label = format!(
            "/{}{}",
            command.name,
            command
                .argument_hint
                .as_deref()
                .map(|hint| format!(" {hint}"))
                .unwrap_or_default()
        );
        let label = sexy_tui_rs::truncate_to_width(
            &raw_label,
            label_width,
            Some(if state.theme.unicode() { "…" } else { "..." }),
        );
        let label = format!(
            "{label}{}",
            " ".repeat(label_width.saturating_sub(visible_width(&label)))
        );
        let description_width =
            usize::from(width).saturating_sub(visible_width(prefix) + visible_width(&label) + 4);
        let description = sexy_tui_rs::truncate_to_width(
            &command.description,
            description_width,
            Some(if state.theme.unicode() { "…" } else { "..." }),
        );
        let row = format!("  {prefix} {label}  {description}");
        lines.push(if selected_row {
            state
                .theme
                .bold(&state.theme.fg("model_accent", &fit_line(&row, width)))
        } else {
            state.theme.fg("muted", &fit_line(&row, width))
        });
    }
    lines
}

fn render_mention_suggestions(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    if max_rows == 0 || state.editor_cursor != state.editor.len() {
        return Vec::new();
    }
    let Some(query) = composer::active_mention(&state.editor) else {
        return Vec::new();
    };

    // When the query looks like a path (contains / or starts with .),
    // do a live filesystem listing instead of searching the pre-built index.
    let looks_like_path = query.contains('/') || query.starts_with('.') || query.contains('\\');
    let matches: Vec<String> = if looks_like_path {
        let Some(root) = &state.workspace else {
            return Vec::new();
        };
        composer::live_path_matches(root, query, 5)
    } else {
        let Some(files) = state.file_index.as_ref() else {
            return Vec::new();
        };
        composer::mention_matches(files, query, 5)
            .into_iter()
            .map(str::to_owned)
            .collect()
    };
    if matches.is_empty() {
        return Vec::new();
    }

    let heading = if state.theme.unicode() {
        "  project files · tab completes"
    } else {
        "  project files - tab completes"
    };
    let mut lines = vec![state.theme.fg("model_accent", heading)];
    let item_rows = max_rows.saturating_sub(1).min(5);
    let available_width = usize::from(width).saturating_sub(2);
    for (index, path) in matches.into_iter().take(item_rows).enumerate() {
        let safe_path = super::sanitize_for_terminal(&path);
        let line = sexy_tui_rs::truncate_to_width(&safe_path, available_width, None);
        let line = format!("  {line}");
        lines.push(if index == 0 {
            state.theme.fg("model_accent", &line)
        } else {
            state.theme.dim(&line)
        });
    }
    lines
}

pub(super) fn render_input_suggestions(
    state: &ShellState,
    width: u16,
    max_rows: usize,
) -> Vec<String> {
    let slash = render_slash_suggestions(state, width, max_rows);
    if slash.is_empty() {
        render_mention_suggestions(state, width, max_rows)
    } else {
        slash
    }
}

pub(super) fn render_pending_steering(
    state: &ShellState,
    width: u16,
    max_rows: usize,
) -> Vec<String> {
    if state.steering_queue.is_empty() || max_rows == 0 {
        return Vec::new();
    }

    let count = state.steering_queue.len();
    let heading = if count == 1 {
        format!("Steering prompt{}queued", semantic_separator(&state.theme))
    } else {
        format!(
            "Steering prompts{}{} queued",
            semantic_separator(&state.theme),
            count
        )
    };
    let mut lines = vec![format!(
        "  {}",
        state.theme.bold(&state.theme.fg("model_accent", &heading))
    )];
    let item_rows = max_rows.saturating_sub(1);
    if item_rows == 0 {
        return lines;
    }

    let visible = state.steering_queue.len().min(item_rows);
    for message in state.steering_queue.iter().take(visible) {
        // Keep each queued message on one predictable row so a burst of
        // steering prompts cannot consume the whole transcript viewport.
        let line_separator = if state.theme.unicode() {
            " ↵ "
        } else {
            " / "
        };
        let compact =
            super::sanitize_for_terminal(&message.display).replace(['\r', '\n'], line_separator);
        let arrow = if state.theme.unicode() { "↳" } else { "->" };
        let prefix = format!("    {} ", state.theme.fg("model_accent", arrow));
        let line = format!("{prefix}{}", state.theme.fg("muted", &compact));
        lines.push(fit_line(&line, width));
    }
    let hidden = state.steering_queue.len().saturating_sub(visible);
    if hidden > 0 {
        lines.push(state.theme.dim(&format!(
            "    {} {hidden} more steering prompts",
            if state.theme.unicode() { "…" } else { "..." }
        )));
    }
    lines.truncate(max_rows);
    lines
}
