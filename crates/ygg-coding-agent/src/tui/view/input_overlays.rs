//! Composer-adjacent slash, mention, and queued-steering overlays.

use sexy_tui_rs::{strip_terminal_sequences, truncate_to_width, visible_width};

use super::{
    activity_elbow, fit_line, fit_prioritized_footer, join_ordinary_metadata,
    sanitize_ordinary_surface_cell, semantic_separator, FooterSegment, ShellState,
    ACTIVITY_DETAIL_INDENT,
};
use crate::commands;
use crate::tui::composer;

/// Provenance remains semantic data until the display projection joins it to
/// untrusted metadata. Raw command identity never includes this label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SlashSuggestionProvenance {
    Builtin,
    Prompt,
    Skill,
    Extension,
}

impl SlashSuggestionProvenance {
    fn label(self) -> Option<&'static str> {
        match self {
            Self::Builtin => None,
            Self::Prompt => Some("prompt"),
            Self::Skill => Some("skill"),
            Self::Extension => Some("extension"),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct InputSlashSuggestion {
    /// Raw command identity used by selection and completion.
    pub(super) name: String,
    /// Raw descriptive metadata projected only at the terminal boundary.
    pub(super) description: String,
    pub(super) argument_hint: Option<String>,
    pub(super) provenance: SlashSuggestionProvenance,
    pub(super) accepts_argument: bool,
}

pub(super) fn input_slash_suggestions(state: &ShellState) -> Vec<InputSlashSuggestion> {
    let Some(query) = state.editor.text().strip_prefix('/') else {
        return Vec::new();
    };
    if query.contains(char::is_whitespace) || query.contains('\n') {
        return Vec::new();
    }
    let mut suggestions = commands::slash_suggestions(state.editor.text())
        .into_iter()
        .map(|command| InputSlashSuggestion {
            name: command.name.to_owned(),
            description: command.description.to_owned(),
            argument_hint: None,
            provenance: SlashSuggestionProvenance::Builtin,
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
            description: template.description.clone(),
            argument_hint: template.argument_hint.clone(),
            provenance: SlashSuggestionProvenance::Prompt,
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
            description: description.clone(),
            argument_hint: None,
            provenance: SlashSuggestionProvenance::Skill,
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
            description: description.clone(),
            argument_hint: None,
            provenance: SlashSuggestionProvenance::Extension,
            accepts_argument: true,
        });
    }
    suggestions
}

fn suggestion_key_hint(state: &ShellState, key: &str, label: &str) -> String {
    format!(
        "{} {}",
        state.theme.bold(&state.theme.fg("model_accent", key)),
        state.theme.fg("muted", label)
    )
}

fn suggestion_separator(state: &ShellState) -> String {
    state.theme.fg("muted", semantic_separator(&state.theme))
}

/// Convert untrusted command metadata into one terminal-safe display cell
/// before it contributes to width, clipping, or trusted theme styling.
fn slash_suggestion_display_cell(state: &ShellState, value: &str) -> String {
    sanitize_ordinary_surface_cell(value, state.theme.unicode())
}

fn slash_suggestion_display_label(state: &ShellState, command: &InputSlashSuggestion) -> String {
    let name = slash_suggestion_display_cell(state, &command.name);
    let argument_hint = command
        .argument_hint
        .as_deref()
        .map(|hint| slash_suggestion_display_cell(state, hint))
        .map(|hint| format!(" {hint}"))
        .unwrap_or_default();
    format!("/{name}{argument_hint}")
}

fn slash_suggestion_display_description(
    state: &ShellState,
    command: &InputSlashSuggestion,
) -> String {
    let description = slash_suggestion_display_cell(state, &command.description);
    command
        .provenance
        .label()
        .map_or(description.clone(), |label| {
            join_ordinary_metadata(&state.theme, &[label, &description])
        })
}

fn truncate_suggestion_display(value: &str, width: usize, ellipsis: &str) -> String {
    // Input is a plain, terminal-safe cell. The ANSI-aware truncator only adds
    // a trusted reset when clipping; strip that synthetic reset before styling
    // so no-colour renderers never receive an escape sequence.
    strip_terminal_sequences(&truncate_to_width(value, width, Some(ellipsis)))
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
    let mut segments = [
        // Scope is visually first but is the least useful segment at compact
        // widths, so its lower rank drops before the optional action tail.
        FooterSegment::optional(state.theme.fg("muted", &scope), 0),
        FooterSegment::primary(suggestion_key_hint(state, navigation_key, "navigate")),
        FooterSegment::optional(suggestion_key_hint(state, select_key, "select"), 2),
        FooterSegment::optional(suggestion_key_hint(state, "esc", "close"), 1),
    ];
    fit_prioritized_footer("  ", &separator, &mut segments, width)
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

    let layout = crate::tui::layout::PresentationLayout::new(&state.theme, width);
    let popup_width = layout.content_width;
    let popup_prefix = " ".repeat(usize::from(layout.inset));

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
        .map(|command| slash_suggestion_display_label(state, command))
        .map(|label| visible_width(&label))
        .max()
        .unwrap_or(1)
        .min(30)
        .min(
            usize::from(popup_width)
                .saturating_sub(marker_width + 1)
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
        let display_label = slash_suggestion_display_label(state, command);
        let label =
            truncate_suggestion_display(&display_label, label_width, state.theme.glyph("ellipsis"));
        let label = format!(
            "{label}{}",
            " ".repeat(label_width.saturating_sub(visible_width(&label)))
        );
        let choice = format!("{prefix} {label}");
        let choice = if selected_row {
            state.theme.bold(&state.theme.fg("model_accent", &choice))
        } else {
            state.theme.fg("foreground", &choice)
        };
        let fixed_width = marker_width + 1 + label_width;
        let description_width = usize::from(popup_width).saturating_sub(fixed_width + 2);
        let display_description = slash_suggestion_display_description(state, command);
        let description = truncate_suggestion_display(
            &display_description,
            description_width,
            state.theme.glyph("ellipsis"),
        );
        let row = if description.is_empty() {
            choice
        } else {
            format!("{choice}  {}", state.theme.fg("muted", &description))
        };
        lines.push(fit_line(
            &format!("{popup_prefix}{}", fit_line(&row, popup_width)),
            width,
        ));
    }
    let footer =
        slash_suggestion_footer(state, popup_width, start, end, suggestions.len(), item_rows);
    lines.push(fit_line(&format!("{popup_prefix}{footer}"), width));
    lines
}

fn render_path_suggestions(state: &ShellState, width: u16, max_rows: usize) -> Vec<String> {
    if max_rows < 2 || state.editor.cursor() != state.editor.text().len() {
        return Vec::new();
    }

    let (heading_label, matches) =
        if let Some(query) = composer::active_mention(state.editor.text()) {
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
        } else if let Some(query) = composer::active_path(state.editor.text()) {
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
        let safe_path = sanitize_ordinary_surface_cell(&path, state.theme.unicode());
        let path =
            truncate_suggestion_display(&safe_path, available_width, state.theme.glyph("ellipsis"));
        let prefix = if index == 0 {
            marker.to_owned()
        } else {
            " ".repeat(marker_width)
        };
        let choice = format!("{prefix} {path}");
        let choice = if index == 0 {
            state.theme.bold(&state.theme.fg("model_accent", &choice))
        } else {
            state.theme.fg("muted", &choice)
        };
        lines.push(fit_line(&format!("  {choice}"), width));
    }

    let separator = suggestion_separator(state);
    let mut segments = [
        FooterSegment::optional(state.theme.fg("muted", heading_label), 0),
        FooterSegment::primary(suggestion_key_hint(state, "tab", "complete")),
    ];
    lines.push(fit_prioritized_footer(
        "  ",
        &separator,
        &mut segments,
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

fn steering_preview_text(state: &ShellState, message: &str) -> String {
    let marker = if state.theme.unicode() {
        " ↵ "
    } else {
        " / "
    };
    super::sanitize_for_terminal(message).replace('\n', marker)
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

    let max_rows = max_rows.min(crate::tui::layout::MAX_STEERING_PREVIEW_ROWS);
    let count = state.steering_queue.len();
    let heading = if count == 1 {
        format!("Steering{}queued", semantic_separator(&state.theme))
    } else {
        format!(
            "Steering{}{} queued",
            semantic_separator(&state.theme),
            count
        )
    };
    let mut lines = vec![fit_line(
        &format!(
            "{ACTIVITY_DETAIL_INDENT}{}",
            state
                .theme
                .bold(&state.theme.model_fg(state.model_lab, &heading))
        ),
        width,
    )];
    if max_rows == 1 {
        return lines;
    }

    let elbow = activity_elbow(&state.theme);
    let plain_prefix = format!("{ACTIVITY_DETAIL_INDENT}{elbow} ");
    let prefix = format!(
        "{ACTIVITY_DETAIL_INDENT}{} ",
        state.theme.model_fg(state.model_lab, elbow)
    );
    let hidden = count.saturating_sub(1);
    let hidden_suffix = if hidden == 0 {
        String::new()
    } else {
        format!("{}+{hidden} more", semantic_separator(&state.theme))
    };
    let available = usize::from(width)
        .saturating_sub(visible_width(&plain_prefix))
        .max(1);
    let preview_budget = available.saturating_sub(visible_width(&hidden_suffix));
    let preview = steering_preview_text(state, &state.steering_queue[0].display);
    let preview = if visible_width(&preview) > preview_budget {
        clipped_steering_content(state, &preview, preview_budget)
    } else {
        preview
    };
    lines.push(fit_line(
        &format!(
            "{prefix}{}{}",
            state.theme.fg("muted", &preview),
            state.theme.fg("muted", &hidden_suffix),
        ),
        width,
    ));
    lines
}
