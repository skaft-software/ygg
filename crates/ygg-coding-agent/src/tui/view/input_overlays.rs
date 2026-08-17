//! Composer-adjacent slash, mention, and queued-steering overlays.

use sexy_tui_rs::{truncate_to_width, visible_width, wrap_text_with_ansi};

use super::{activity_elbow, fit_line, semantic_separator, ShellState, ACTIVITY_DETAIL_INDENT};
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
        .skill_commands
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
            description: format!("skill · {description}"),
            argument_hint: None,
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

fn suggestion_key_hint(state: &ShellState, key: &str, label: &str) -> String {
    format!(
        "{} {}",
        state
            .theme
            .bold(&state.theme.model_fg(state.model_lab, key)),
        state.theme.fg("muted", label)
    )
}

fn suggestion_separator(state: &ShellState) -> String {
    state.theme.fg("muted", semantic_separator(&state.theme))
}

fn slash_suggestion_footer(
    state: &ShellState,
    width: u16,
    start: usize,
    end: usize,
    total: usize,
    visible_rows: usize,
) -> String {
    let scope = if total > visible_rows {
        let range_separator = if state.theme.unicode() { "–" } else { "-" };
        format!(
            "commands {}{range_separator}{end}/{total}",
            start.saturating_add(1)
        )
    } else {
        "commands".to_owned()
    };
    let (navigation_key, select_key) = if state.theme.unicode() {
        ("↑↓", "↵")
    } else {
        ("up/down", "enter")
    };
    let separator = suggestion_separator(state);
    fit_line(
        &format!(
            "  {}{separator}{}{separator}{}{separator}{}",
            state.theme.fg("muted", &scope),
            suggestion_key_hint(state, navigation_key, "navigate"),
            suggestion_key_hint(state, select_key, "select"),
            suggestion_key_hint(state, "esc", "close"),
        ),
        width,
    )
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

    // Keep one compact hint row below the choices. Moving the metadata to the
    // footer makes autocomplete read as an inline continuation of the composer
    // rather than a second panel with its own heading.
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

    let marker = state.theme.glyph("prompt");
    let marker_width = visible_width(marker).max(1);
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
        .min(
            usize::from(width)
                .saturating_sub(2 + marker_width + 1)
                .max(1),
        );
    let mut lines = Vec::with_capacity(end.saturating_sub(start) + 1);
    for (index, command) in suggestions[start..end].iter().enumerate() {
        let absolute = start + index;
        let selected_row = absolute == selected;
        let prefix = if selected_row {
            marker.to_owned()
        } else {
            " ".repeat(marker_width)
        };
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
        let choice = format!("{prefix} {label}");
        let choice = if selected_row {
            state
                .theme
                .bold(&state.theme.model_fg(state.model_lab, &choice))
        } else {
            state.theme.fg("foreground", &choice)
        };
        let fixed_width = 2 + marker_width + 1 + label_width;
        let description_width = usize::from(width).saturating_sub(fixed_width + 2);
        let description = sexy_tui_rs::truncate_to_width(
            &command.description,
            description_width,
            Some(if state.theme.unicode() { "…" } else { "..." }),
        );
        let row = if description.is_empty() {
            format!("  {choice}")
        } else {
            format!("  {choice}  {}", state.theme.fg("muted", &description))
        };
        lines.push(fit_line(&row, width));
    }
    lines.push(slash_suggestion_footer(
        state,
        width,
        start,
        end,
        suggestions.len(),
        item_rows,
    ));
    lines
}

fn render_path_suggestions(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    if max_rows < 2 || state.editor_cursor != state.editor.len() {
        return Vec::new();
    }

    let (heading_label, matches) = if let Some(query) = composer::active_mention(&state.editor) {
        if composer::is_path_query(query) {
            let Some(root) = &state.workspace else {
                return Vec::new();
            };
            let matches = composer::path_matches(root, query, 5)
                .into_iter()
                .map(|suggestion| suggestion.completion)
                .collect();
            ("paths", matches)
        } else {
            let Some(files) = state.file_index.as_ref() else {
                return Vec::new();
            };
            let matches = composer::mention_matches(files, query, 5)
                .into_iter()
                .map(str::to_owned)
                .collect();
            ("project files", matches)
        }
    } else if let Some(query) = composer::active_path(&state.editor) {
        let Some(root) = &state.workspace else {
            return Vec::new();
        };
        let matches = composer::path_matches(root, query, 5)
            .into_iter()
            .map(|suggestion| suggestion.completion)
            .collect();
        ("paths", matches)
    } else {
        return Vec::new();
    };
    let matches: Vec<String> = matches;
    if matches.is_empty() {
        return Vec::new();
    }

    let item_rows = max_rows.saturating_sub(1).min(5);
    let marker = state.theme.glyph("prompt");
    let marker_width = visible_width(marker).max(1);
    let available_width = usize::from(width)
        .saturating_sub(2 + marker_width + 1)
        .max(1);
    let mut lines = Vec::with_capacity(item_rows.saturating_add(1));
    for (index, path) in matches.into_iter().take(item_rows).enumerate() {
        let safe_path = super::sanitize_for_terminal(&path);
        let path = sexy_tui_rs::truncate_to_width(&safe_path, available_width, None);
        let prefix = if index == 0 {
            marker.to_owned()
        } else {
            " ".repeat(marker_width)
        };
        let choice = format!("{prefix} {path}");
        let choice = if index == 0 {
            state
                .theme
                .bold(&state.theme.model_fg(state.model_lab, &choice))
        } else {
            state.theme.fg("muted", &choice)
        };
        lines.push(fit_line(&format!("  {choice}"), width));
    }

    let separator = suggestion_separator(state);
    lines.push(fit_line(
        &format!(
            "  {}{separator}{}",
            state.theme.fg("muted", heading_label),
            suggestion_key_hint(state, "tab", "complete"),
        ),
        width,
    ));
    lines
}

pub(super) fn render_input_suggestions(
    state: &ShellState,
    width: u16,
    max_rows: usize,
) -> Vec<String> {
    let slash = render_slash_suggestions(state, width, max_rows);
    if slash.is_empty() {
        render_path_suggestions(state, width, max_rows)
    } else {
        slash
    }
}

fn steering_message_rows(state: &ShellState, message: &str, content_width: usize) -> Vec<String> {
    let safe = super::sanitize_for_terminal(message);
    let newline_marker = if state.theme.unicode() {
        " ↵\n"
    } else {
        " /\n"
    };
    wrap_text_with_ansi(&safe.replace('\n', newline_marker), content_width.max(1))
}

fn clipped_steering_content(state: &ShellState, content: &str, width: usize) -> String {
    let suffix = if state.theme.unicode() {
        " …"
    } else {
        " ..."
    };
    let suffix_width = visible_width(suffix);
    if width <= suffix_width {
        return truncate_to_width(suffix.trim_start(), width, Some(""));
    }
    let body = truncate_to_width(content, width - suffix_width, Some(""));
    format!("{}{suffix}", body.trim_end())
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
    let mut lines = vec![fit_line(
        &format!(
            "{ACTIVITY_DETAIL_INDENT}{}",
            state.theme.bold(&state.theme.fg("model_accent", &heading))
        ),
        width,
    )];
    let item_rows = max_rows.saturating_sub(1);
    if item_rows == 0 {
        return lines;
    }

    let elbow = activity_elbow(&state.theme);
    let plain_prefix = format!("{ACTIVITY_DETAIL_INDENT}{elbow} ");
    let prefix_width = visible_width(&plain_prefix);
    let content_width = usize::from(width).saturating_sub(prefix_width).max(1);
    let first_prefix = format!(
        "{ACTIVITY_DETAIL_INDENT}{} ",
        state.theme.fg("model_accent", elbow)
    );
    let continuation = " ".repeat(prefix_width);
    let wrapped = state
        .steering_queue
        .iter()
        .map(|message| steering_message_rows(state, &message.display, content_width))
        .collect::<Vec<_>>();
    let total_message_rows = wrapped.iter().map(Vec::len).sum::<usize>();

    let needs_overflow = total_message_rows > item_rows;
    // On a severely constrained viewport, showing one useful preview is better
    // than spending the only item row restating the count already in the heading.
    let summary_rows = usize::from(needs_overflow && item_rows > 1);
    let content_budget = item_rows.saturating_sub(summary_rows);
    let visible_messages = wrapped.len().min(content_budget);
    let mut allocations = vec![0usize; visible_messages];

    // Every visible prompt gets a preview before any one prompt claims a second
    // row. Extra rows are then shared round-robin in queue order.
    allocations.fill(1);
    let mut unallocated = content_budget.saturating_sub(visible_messages);
    while unallocated > 0 {
        let mut made_progress = false;
        for (index, allocation) in allocations.iter_mut().enumerate() {
            if *allocation < wrapped[index].len() {
                *allocation += 1;
                unallocated -= 1;
                made_progress = true;
                if unallocated == 0 {
                    break;
                }
            }
        }
        if !made_progress {
            break;
        }
    }

    let mut clipped_messages = 0usize;
    for (message_index, allocation) in allocations.iter().copied().enumerate() {
        let message_rows = &wrapped[message_index];
        let clipped = allocation < message_rows.len();
        clipped_messages += usize::from(clipped);
        for (row_index, content) in message_rows.iter().take(allocation).enumerate() {
            let content = if clipped && row_index + 1 == allocation {
                clipped_steering_content(state, content, content_width)
            } else {
                content.clone()
            };
            let prefix = if row_index == 0 {
                first_prefix.as_str()
            } else {
                continuation.as_str()
            };
            lines.push(fit_line(
                &format!("{prefix}{}", state.theme.fg("muted", &content)),
                width,
            ));
        }
    }

    let hidden_messages = wrapped.len().saturating_sub(visible_messages);
    if summary_rows > 0 {
        let mut details = Vec::with_capacity(2);
        if clipped_messages > 0 {
            details.push(format!(
                "{clipped_messages} prompt{} clipped",
                if clipped_messages == 1 { "" } else { "s" }
            ));
        }
        if hidden_messages > 0 {
            details.push(format!("{hidden_messages} more queued"));
        }
        let ellipsis = if state.theme.unicode() { "…" } else { "..." };
        lines.push(fit_line(
            &state.theme.dim(&format!(
                "{ACTIVITY_DETAIL_INDENT}{ellipsis} {}",
                details.join(semantic_separator(&state.theme))
            )),
            width,
        ));
    }

    debug_assert!(lines.len() <= max_rows);
    lines
}
