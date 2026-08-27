//! Select-list panel filtering, layout, and rendering.

use std::cmp::Ordering;
use std::time::{Duration, Instant, SystemTime};

use sexy_tui_rs::{visible_width, wrap_text_with_ansi, CURSOR_MARKER};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    fit_line, subdued_text, ForkMessage, MessagePicker, Panel, PickerScope, PickerSort,
    PickerState, ShellState,
};
use crate::tui::fuzzy::{fuzzy_match, parse_search_query, SearchMode, TokenKind};
use crate::tui::theme::YggTheme;

/// Indices of the items matching the current filter. Every whitespace-delimited
/// term must appear in either the label or description, case-insensitively.
pub(super) fn filtered_indices(
    items: &[String],
    descriptions: &[Option<String>],
    filter: &str,
) -> Vec<usize> {
    let needles = filter
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>();
    items
        .iter()
        .enumerate()
        .filter(|(index, item)| {
            if needles.is_empty() {
                return true;
            }
            let mut searchable = item.to_lowercase();
            if let Some(description) = descriptions
                .get(*index)
                .and_then(|description| description.as_deref())
            {
                searchable.push(' ');
                searchable.push_str(&description.to_lowercase());
            }
            needles.iter().all(|needle| searchable.contains(needle))
        })
        .map(|(index, _)| index)
        .collect()
}

fn normalize_search_text(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn session_search_text(meta: &crate::session_store::SessionMeta) -> String {
    format!(
        "{} {} {} {} {} {}",
        meta.id,
        meta.name.as_deref().unwrap_or_default(),
        meta.title,
        meta.tags.join(" "),
        meta.path.display(),
        meta.workspace
            .as_deref()
            .map_or_else(String::new, |workspace| workspace.display().to_string()),
    )
}

fn match_session_search(
    meta: &crate::session_store::SessionMeta,
    query: &crate::tui::fuzzy::ParsedSearchQuery,
) -> Option<f64> {
    if query.error.is_some() {
        return None;
    }
    if query.is_empty() {
        return Some(0.0);
    }
    let haystack = session_search_text(meta);
    match query.mode {
        SearchMode::Regex => {
            let regex = query.regex.as_ref()?;
            regex
                .find(&haystack)
                .map(|matched| matched.start() as f64 * 0.1)
        }
        SearchMode::Tokens => {
            let mut score = 0.0;
            let mut normalized = None;
            for token in &query.tokens {
                match token.kind {
                    TokenKind::Phrase => {
                        let normalized_haystack =
                            normalized.get_or_insert_with(|| normalize_search_text(&haystack));
                        let phrase = normalize_search_text(&token.value);
                        if phrase.is_empty() {
                            continue;
                        }
                        let position = normalized_haystack.find(&phrase)?;
                        score += position as f64 * 0.1;
                    }
                    TokenKind::Fuzzy => {
                        let matched = fuzzy_match(&token.value, &haystack);
                        if !matched.matches {
                            return None;
                        }
                        score += matched.score;
                    }
                }
            }
            Some(score)
        }
    }
}

/// Return the displayed session indices after named filtering and sorting.
pub(super) fn session_picker_ordering(picker: &PickerState) -> Vec<usize> {
    let rows = picker.active_rows();
    let query = parse_search_query(&picker.filter);
    let mut scored = rows
        .iter()
        .enumerate()
        .filter_map(|(index, meta)| {
            if picker.named_only
                && !meta
                    .name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
            {
                return None;
            }
            let score = match_session_search(meta, &query)?;
            Some((index, score))
        })
        .collect::<Vec<_>>();

    match picker.sort {
        // Store discovery is already newest-first. Keeping this order makes a
        // filtered recent view stable and avoids a second filesystem sort.
        PickerSort::Recent => {}
        PickerSort::Name => scored.sort_by(|(left, left_score), (right, right_score)| {
            let left_meta = &rows[*left];
            let right_meta = &rows[*right];
            left_meta
                .title
                .to_ascii_lowercase()
                .cmp(&right_meta.title.to_ascii_lowercase())
                .then_with(|| right_meta.modified.cmp(&left_meta.modified))
                .then_with(|| left_meta.id.cmp(&right_meta.id))
                .then_with(|| {
                    left_score
                        .partial_cmp(right_score)
                        .unwrap_or(Ordering::Equal)
                })
        }),
        PickerSort::Messages => scored.sort_by(|(left, left_score), (right, right_score)| {
            let left_meta = &rows[*left];
            let right_meta = &rows[*right];
            right_meta
                .message_count
                .cmp(&left_meta.message_count)
                .then_with(|| {
                    left_score
                        .partial_cmp(right_score)
                        .unwrap_or(Ordering::Equal)
                })
                .then_with(|| right_meta.modified.cmp(&left_meta.modified))
                .then_with(|| left_meta.id.cmp(&right_meta.id))
        }),
    }
    scored.into_iter().map(|(index, _)| index).collect()
}

fn shorten_home_path(path: &std::path::Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return path.display().to_string();
    };
    if path == home {
        return "~".to_owned();
    }
    path.strip_prefix(&home).map_or_else(
        |_| path.display().to_string(),
        |relative| {
            if relative.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", relative.display())
            }
        },
    )
}

fn format_age(modified: SystemTime, now: SystemTime) -> String {
    let seconds = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    if seconds < 60 {
        return "now".to_owned();
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d");
    }
    if days < 30 {
        return format!("{}w", days / 7);
    }
    if days < 365 {
        return format!("{}mo", days / 30);
    }
    format!("{}y", days / 365)
}

fn picker_header_title(picker: &PickerState) -> String {
    match picker.scope {
        PickerScope::Current => "Resume Session (Current Folder)".to_owned(),
        PickerScope::All => "Resume Session (All)".to_owned(),
    }
}

fn picker_scope_text(theme: &YggTheme, picker: &PickerState) -> String {
    let (current, all) = if theme.unicode() {
        ("◉ Current Folder", "○ All")
    } else {
        ("[*] Current Folder", "[ ] All")
    };
    let current = if picker.scope == PickerScope::Current {
        theme.fg("accent", current)
    } else {
        subdued_text(theme, current)
    };
    let all = if picker.scope == PickerScope::All {
        theme.fg("accent", all)
    } else {
        subdued_text(theme, all)
    };
    format!(
        "{current} | {all}  {}  Sort: {}",
        if picker.named_only {
            "Name: Named"
        } else {
            "Name: All"
        },
        picker.sort.label()
    )
}

fn picker_header_line(state: &ShellState, picker: &PickerState, width: u16) -> String {
    let width = usize::from(width);
    let left = state.theme.bold(&picker_header_title(picker));
    let right = picker_scope_text(&state.theme, picker);
    let right = sexy_tui_rs::truncate_to_width(&right, width, Some("…"));
    let gap = width
        .saturating_sub(visible_width(&left))
        .saturating_sub(visible_width(&right))
        .max(1);
    fit_line(&format!("{left}{}{}", " ".repeat(gap), right), width as u16)
}

fn picker_filter_line(state: &ShellState, picker: &PickerState, width: u16) -> String {
    let (label, value) = match picker.rename.as_deref() {
        Some(value) => ("Rename", value),
        None => ("Filter", picker.filter.as_str()),
    };
    let width = usize::from(width);
    let prefix = format!("  {}  ", subdued_text(&state.theme, label));
    let available = width.saturating_sub(visible_width(&prefix));
    let ellipsis = if state.theme.unicode() { "…" } else { "..." };
    let value = panel_cell(value);
    if value.is_empty() {
        let placeholder = if label == "Rename" {
            "enter a session name"
        } else {
            "type to filter"
        };
        let placeholder = sexy_tui_rs::truncate_to_width(placeholder, available, Some(ellipsis));
        format!(
            "{prefix}{CURSOR_MARKER}{}",
            subdued_text(&state.theme, &placeholder)
        )
    } else {
        let query = sexy_tui_rs::truncate_to_width(&value, available, Some(ellipsis));
        format!(
            "{prefix}{}{CURSOR_MARKER}",
            state.theme.fg("foreground", &query)
        )
    }
}

fn picker_hints(state: &ShellState, picker: &PickerState, width: u16) -> (String, String) {
    let now = Instant::now();
    let first = if picker.confirming_delete {
        state
            .theme
            .fg("error", "Delete session? Enter confirm · Esc cancel")
    } else if picker.rename.is_some() {
        subdued_text(&state.theme, "Enter save · Esc cancel")
    } else if let Some((message, _expires)) = picker
        .message
        .as_ref()
        .filter(|(_, expires)| *expires > now)
    {
        let tone = if message.starts_with("Cannot") || message.starts_with("Failed") {
            "error"
        } else {
            "accent"
        };
        state.theme.fg(tone, &panel_cell(message))
    } else {
        "tab scope · re:<pattern> regex · \"phrase\" exact".to_owned()
    };
    let second = if picker.confirming_delete || picker.rename.is_some() {
        String::new()
    } else {
        "^s sort · ^n named · del delete · ^p path (on/off) · ^r rename".to_owned()
    };
    (
        fit_line(&first, width),
        fit_line(&subdued_text(&state.theme, &second), width),
    )
}

fn picker_workspace(meta: &crate::session_store::SessionMeta) -> String {
    meta.workspace.as_deref().map_or_else(
        || {
            meta.path
                .parent()
                .and_then(|path| path.file_name())
                .map_or_else(
                    || "(unknown workspace)".to_owned(),
                    |name| name.to_string_lossy().into(),
                )
        },
        shorten_home_path,
    )
}

fn render_picker_row(
    state: &ShellState,
    meta: &crate::session_store::SessionMeta,
    picker: &PickerState,
    selected: bool,
    confirming: bool,
    width: u16,
) -> String {
    let is_current = picker.current_session_path.as_ref() == Some(&meta.path);
    let mut label = String::new();
    if meta.pinned {
        label.push_str(state.theme.glyph("bullet"));
        label.push(' ');
    }
    label.push_str(&meta.title);
    if meta.forked_from_session_id.is_some() {
        label.push_str(" (fork)");
    }
    if is_current {
        label.push_str(" (current)");
    }
    let now = SystemTime::now();
    let mut right = if picker.scope == PickerScope::All {
        format!(
            "{} · {}",
            picker_workspace(meta),
            format_age(meta.modified, now)
        )
    } else {
        format_age(meta.modified, now)
    };
    if picker.show_path {
        right.push_str(" · ");
        right.push_str(&shorten_home_path(&meta.path));
    }
    let right = panel_cell(&right);
    let cursor = if selected {
        state.theme.fg("accent", "› ")
    } else {
        "  ".to_owned()
    };
    let right_width = visible_width(&right);
    let available = usize::from(width)
        .saturating_sub(visible_width(&cursor))
        .saturating_sub(right_width)
        .saturating_sub(1);
    let label = sexy_tui_rs::truncate_to_width(&panel_cell(&label), available.max(1), Some("…"));
    let label = if confirming {
        state.theme.fg("error", &label)
    } else if is_current {
        state.theme.fg("accent", &label)
    } else if meta.name.is_some() {
        state.theme.fg("warning", &label)
    } else {
        label
    };
    let label = if selected {
        state.theme.bold(&label)
    } else {
        label
    };
    let spacing = usize::from(width)
        .saturating_sub(visible_width(&cursor))
        .saturating_sub(visible_width(&label))
        .saturating_sub(right_width)
        .max(1);
    fit_line(
        &format!(
            "{cursor}{label}{}{}",
            " ".repeat(spacing),
            subdued_text(&state.theme, &right)
        ),
        width,
    )
}

fn tree_prefix(index: usize, total: usize, unicode: bool) -> &'static str {
    if total <= 1 {
        ""
    } else if unicode {
        if index + 1 == total {
            "└─ "
        } else {
            "├─ "
        }
    } else if index + 1 == total {
        "`- "
    } else {
        "|- "
    }
}

fn render_message_item(
    state: &ShellState,
    message: &ForkMessage,
    index: usize,
    total: usize,
    selected: bool,
    width: u16,
) -> Vec<String> {
    let display = if message.whole_conversation {
        "Whole conversation".to_owned()
    } else {
        message.text.replace(['\n', '\r'], " ")
    };
    let display = panel_cell(&display);
    let prefix = tree_prefix(index, total, state.theme.unicode());
    let cursor = if selected {
        state.theme.fg("accent", "› ")
    } else {
        "  ".to_owned()
    };
    let available = usize::from(width)
        .saturating_sub(visible_width(&cursor))
        .saturating_sub(visible_width(prefix));
    let display = sexy_tui_rs::truncate_to_width(display.trim(), available.max(1), Some("…"));
    let display = if selected {
        state.theme.bold(&display)
    } else {
        display
    };
    vec![
        fit_line(
            &format!("{cursor}{}{display}", subdued_text(&state.theme, prefix)),
            width,
        ),
        fit_line(
            &subdued_text(
                &state.theme,
                &format!("  Message {} of {}", index + 1, total),
            ),
            width,
        ),
        String::new(),
    ]
}

fn session_empty_message(picker: &PickerState) -> String {
    if picker.named_only {
        match picker.scope {
            PickerScope::Current => {
                "  No named sessions in current workspace. Press ^n to show all, or tab to view all."
                    .to_owned()
            }
            PickerScope::All => "  No named sessions found. Press ^n to show all.".to_owned(),
        }
    } else {
        match picker.scope {
            PickerScope::Current => {
                "  No sessions in current workspace. Press tab to view all.".to_owned()
            }
            PickerScope::All => "  No sessions found".to_owned(),
        }
    }
}

fn render_session_picker(
    state: &ShellState,
    picker: &PickerState,
    width: u16,
    max_rows: usize,
    rule: &str,
) -> Vec<String> {
    let show_borders = state.theme.layout_for_width(width).show_panel_borders && max_rows >= 6;
    let border_rows = usize::from(show_borders) * 2;
    let mut lines = Vec::with_capacity(max_rows);
    if show_borders {
        lines.push(subdued_text(&state.theme, rule));
    }
    lines.push(picker_header_line(state, picker, width));
    if max_rows >= 2 {
        lines.push(picker_filter_line(state, picker, width));
    }
    if max_rows >= 3 {
        let (first, second) = picker_hints(state, picker, width);
        lines.push(first);
        if max_rows >= 4 {
            lines.push(second);
        }
    }

    let body_rows = max_rows.saturating_sub(4 + border_rows);
    if body_rows > 0 {
        let ordering = session_picker_ordering(picker);
        if picker.scope == PickerScope::All && picker.all_rows.is_none() {
            lines.push(fit_line(
                &subdued_text(&state.theme, "  Loading all workspaces…"),
                width,
            ));
        } else if ordering.is_empty() {
            lines.push(fit_line(
                &subdued_text(&state.theme, &session_empty_message(picker)),
                width,
            ));
        } else {
            let show_indicator = ordering.len() > body_rows;
            let visible_rows = body_rows.saturating_sub(usize::from(show_indicator));
            let window = panel_window(picker.selected, ordering.len(), visible_rows);
            for position in window {
                if let Some(meta) = picker.active_rows().get(ordering[position]) {
                    lines.push(render_picker_row(
                        state,
                        meta,
                        picker,
                        position == picker.selected,
                        picker.confirming_delete && position == picker.selected,
                        width,
                    ));
                }
            }
            if show_indicator {
                let selected = picker.selected.min(ordering.len().saturating_sub(1));
                lines.push(fit_line(
                    &subdued_text(
                        &state.theme,
                        &format!("  ({}/{})", selected + 1, ordering.len()),
                    ),
                    width,
                ));
            }
        }
    }
    if show_borders {
        lines.push(subdued_text(&state.theme, rule));
    }
    lines.truncate(max_rows);
    lines
}

fn render_message_picker(
    state: &ShellState,
    picker: &MessagePicker,
    width: u16,
    max_rows: usize,
    rule: &str,
) -> Vec<String> {
    let show_borders = state.theme.layout_for_width(width).show_panel_borders && max_rows >= 5;
    let border_rows = usize::from(show_borders) * 2;
    let mut lines = Vec::with_capacity(max_rows);
    if show_borders {
        lines.push(subdued_text(&state.theme, rule));
    }
    lines.push(fit_line(&state.theme.bold("Fork from Message"), width));
    lines.push(fit_line(
        &subdued_text(
            &state.theme,
            "Select a message to copy its path into a new session",
        ),
        width,
    ));
    if max_rows >= 3 {
        lines.push(fit_line(
            &subdued_text(&state.theme, "↑↓ select · enter fork · esc cancel"),
            width,
        ));
    }
    let body_rows = max_rows.saturating_sub(3 + border_rows);
    if body_rows > 0 {
        if picker.messages.is_empty() {
            lines.push(fit_line(
                &subdued_text(&state.theme, "  No user messages found"),
                width,
            ));
        } else {
            let total = picker.messages.len();
            let show_indicator = total.saturating_mul(3) > body_rows;
            let item_rows = body_rows.saturating_sub(usize::from(show_indicator));
            let visible = item_rows / 3;
            let window = panel_window(picker.selected, total, visible.min(total));
            for index in window {
                if let Some(message) = picker.messages.get(index) {
                    lines.extend(render_message_item(
                        state,
                        message,
                        index,
                        total,
                        index == picker.selected,
                        width,
                    ));
                }
            }
            if show_indicator {
                let selected = picker.selected.min(total.saturating_sub(1));
                lines.push(fit_line(
                    &subdued_text(&state.theme, &format!("  ({}/{})", selected + 1, total)),
                    width,
                ));
            }
        }
    }
    if show_borders {
        lines.push(subdued_text(&state.theme, rule));
    }
    lines.truncate(max_rows);
    lines
}

fn panel_cell(text: &str) -> String {
    super::sanitize_for_terminal(text).replace('\n', " ")
}

/// `styled` text was already sanitized at its producing boundary and carries
/// trusted theme ANSI that must survive wrapping.
pub(super) fn document_visual_lines_styled(text: &str, width: u16, styled: bool) -> Vec<String> {
    let inset = usize::from(width >= 5) * 2;
    let available = usize::from(width)
        .saturating_sub(inset.saturating_mul(2))
        .max(1);
    let text = if styled {
        text.to_owned()
    } else {
        super::sanitize_for_terminal(text)
    };
    let mut lines = Vec::new();
    for source in text.split('\n') {
        let wrapped = wrap_text_with_ansi(source, available);
        if wrapped.is_empty() {
            lines.push(" ".repeat(inset));
        } else {
            lines.extend(
                wrapped
                    .into_iter()
                    .map(|line| format!("{}{line}", " ".repeat(inset))),
            );
        }
    }
    lines
}

pub(super) fn document_visual_row_count_styled(text: &str, width: u16, styled: bool) -> usize {
    document_visual_lines_styled(text, width, styled).len()
}

fn is_confirmation_panel(action: &super::PanelAction) -> bool {
    matches!(action, super::PanelAction::Confirmation)
}

fn panel_header(
    theme: &YggTheme,
    title: &str,
    selected: usize,
    matches: usize,
    show_position: bool,
    width: u16,
) -> String {
    let terminal_width = width;
    let width = usize::from(width);
    let inset = usize::from(width >= 5) * 2;
    let available = width.saturating_sub(inset.saturating_mul(2));
    let title = panel_cell(
        if width < 28 && title.eq_ignore_ascii_case("select model") {
            "Models"
        } else {
            title
        },
    );
    let position = if show_position {
        if matches == 0 {
            "0/0".to_owned()
        } else {
            format!("{}/{}", selected.min(matches - 1) + 1, matches)
        }
    } else {
        String::new()
    };
    let gap = available
        .saturating_sub(visible_width(&title))
        .saturating_sub(visible_width(&position));
    let line = if show_position {
        format!(
            "{}{}{}{}{}",
            " ".repeat(inset),
            theme.bold(&title),
            " ".repeat(gap.max(1)),
            subdued_text(theme, &position),
            " ".repeat(inset)
        )
    } else {
        format!(
            "{}{}{}",
            " ".repeat(inset),
            theme.bold(&title),
            " ".repeat(inset)
        )
    };
    fit_line(&line, terminal_width)
}

fn panel_filter_line(theme: &YggTheme, filter: &str, width: u16) -> String {
    let width = usize::from(width);
    let label_text = if width >= 12 {
        "Filter"
    } else if width >= 4 {
        "F"
    } else {
        ""
    };
    let label = subdued_text(theme, label_text);
    let prefix = if label_text.is_empty() {
        String::new()
    } else if label_text == "F" {
        format!("{label} ")
    } else {
        format!("  {label}  ")
    };
    let available = width.saturating_sub(visible_width(&prefix));
    let filter = panel_cell(filter);
    if filter.is_empty() {
        let placeholder = sexy_tui_rs::truncate_to_width(
            "type to filter",
            available,
            Some(if theme.unicode() { "…" } else { "..." }),
        );
        format!(
            "{prefix}{CURSOR_MARKER}{}",
            subdued_text(theme, &placeholder)
        )
    } else {
        let ellipsis = if theme.unicode() { "…" } else { "..." };
        let query = if visible_width(&filter) <= available {
            filter
        } else {
            let ellipsis_width = visible_width(ellipsis).min(available);
            let suffix_budget = available.saturating_sub(ellipsis_width);
            let mut suffix_start = filter.len();
            let mut suffix_width: usize = 0;
            for (index, grapheme) in filter.grapheme_indices(true).rev() {
                let grapheme_width = visible_width(grapheme);
                if suffix_width.saturating_add(grapheme_width) > suffix_budget {
                    break;
                }
                suffix_start = index;
                suffix_width += grapheme_width;
            }
            let visible_ellipsis = sexy_tui_rs::truncate_to_width(ellipsis, available, Some(""));
            format!("{visible_ellipsis}{}", &filter[suffix_start..])
        };
        format!("{prefix}{}{CURSOR_MARKER}", theme.fg("foreground", &query))
    }
}

fn panel_window(selected: usize, matches: usize, visible: usize) -> std::ops::Range<usize> {
    if matches == 0 || visible == 0 {
        return 0..0;
    }
    let selected = selected.min(matches - 1);
    let start = selected
        .saturating_sub(visible / 2)
        .min(matches.saturating_sub(visible));
    start..start.saturating_add(visible).min(matches)
}

fn panel_label_width(
    items: &[String],
    descriptions: &[Option<String>],
    filtered: &[usize],
    width: u16,
) -> Option<usize> {
    let content_width = usize::from(width).saturating_sub(4);
    let max_label = filtered
        .iter()
        .map(|index| visible_width(&panel_cell(&items[*index])))
        .max()
        .unwrap_or(0);
    let has_description = filtered.iter().any(|index| {
        descriptions
            .get(*index)
            .and_then(|description| description.as_deref())
            .is_some_and(|description| !description.is_empty())
    });
    if !has_description || content_width < 42 {
        return None;
    }
    let label_width = max_label.clamp(22, 44).min(content_width * 45 / 100);
    (content_width.saturating_sub(label_width + 2) >= 18).then_some(label_width)
}

fn render_panel_item(
    state: &ShellState,
    item: &str,
    description: Option<&str>,
    is_selected: bool,
    label_width: Option<usize>,
    width: u16,
) -> String {
    let item = panel_cell(item);
    let marker = state.theme.glyph("prompt");
    let prefix = if is_selected {
        format!("  {} ", state.theme.fg("model_accent", marker))
    } else {
        "    ".to_owned()
    };
    let available = usize::from(width).saturating_sub(visible_width(&prefix));
    let ellipsis = if state.theme.unicode() { "…" } else { "..." };

    let label = if let Some(label_width) = label_width {
        sexy_tui_rs::truncate_to_width(&item, label_width, Some(ellipsis))
    } else {
        sexy_tui_rs::truncate_to_width(&item, available, Some(ellipsis))
    };
    let label = if is_selected {
        state.theme.bold(&state.theme.fg("model_accent", &label))
    } else {
        label
    };

    let mut line = format!("{prefix}{label}");
    if let (Some(label_width), Some(description)) = (label_width, description) {
        let padding = label_width.saturating_sub(visible_width(&item));
        let description_width = available.saturating_sub(label_width + 2);
        let description = sexy_tui_rs::truncate_to_width(
            &panel_cell(description),
            description_width,
            Some(ellipsis),
        );
        line.push_str(&" ".repeat(padding + 2));
        line.push_str(&subdued_text(&state.theme, &description));
    }
    fit_line(&line, width)
}

/// How many rows the active panel needs (capped so it cannot squeeze the
/// transcript to zero).
#[cfg(test)]
fn panel_rows(state: &ShellState, width: u16) -> usize {
    let Some(ref panel) = state.panel else {
        return 0;
    };
    let term_rows = usize::from(state.size.1.max(5));
    let max_panel = term_rows.saturating_sub(4); // leave room for composer + footer
    match panel {
        Panel::SelectList {
            items,
            descriptions,
            filter,
            action,
            ..
        } => {
            let confirmation = is_confirmation_panel(action);
            // `(no matches)` still occupies one body row. Confirmation panels
            // have exactly two actions, so they do not need filter chrome or a
            // count in their heading.
            let body = filtered_indices(items, descriptions, filter).len().max(1);
            let border_rows = usize::from(
                !confirmation
                    && state.theme.layout_for_width(width).show_panel_borders
                    && max_panel >= 4,
            ) * 2;
            let chrome_rows = if confirmation { 1 } else { 2 };
            (body + chrome_rows + border_rows).min(max_panel)
        }
        Panel::SessionPicker { picker } => {
            let body = session_picker_ordering(picker).len().max(1);
            let border_rows = usize::from(
                state.theme.layout_for_width(width).show_panel_borders && max_panel >= 6,
            ) * 2;
            (body + 4 + border_rows).min(max_panel)
        }
        Panel::MessagePicker { picker } => {
            let body = picker.messages.len().saturating_mul(3).max(1);
            let border_rows = usize::from(
                state.theme.layout_for_width(width).show_panel_borders && max_panel >= 5,
            ) * 2;
            (body + 3 + border_rows).min(max_panel)
        }
        Panel::ReadOnlyDocument { text, styled, .. } => {
            let border_rows = usize::from(
                state.theme.layout_for_width(width).show_panel_borders && max_panel >= 5,
            ) * 2;
            (document_visual_row_count_styled(text, width, *styled) + 2 + border_rows)
                .min(max_panel)
        }
    }
}

#[cfg(test)]
pub(super) fn render_panel(state: &ShellState, width: u16) -> Vec<String> {
    render_panel_with_limit(state, width, panel_rows(state, width))
}

pub(super) fn document_body_rows(state: &ShellState, width: u16, max_rows: usize) -> usize {
    let show_borders = state.theme.layout_for_width(width).show_panel_borders && max_rows >= 5;
    let border_rows = usize::from(show_borders) * 2;
    max_rows.saturating_sub(border_rows + 2).max(1)
}

pub(super) fn render_panel_with_limit(
    state: &ShellState,
    width: u16,
    max_rows: usize,
) -> Vec<String> {
    let Some(ref panel) = state.panel else {
        return Vec::new();
    };
    if max_rows == 0 {
        return Vec::new();
    }
    let w = usize::from(width).max(1);
    let rule = state.theme.glyph("horizontal").repeat(w);
    let dim = |s: &str| subdued_text(&state.theme, s);

    match panel {
        Panel::SelectList {
            title,
            items,
            descriptions,
            selected,
            filter,
            action,
        } => {
            let confirmation = is_confirmation_panel(action);
            let filtered = filtered_indices(items, descriptions, filter);
            let header = panel_header(
                &state.theme,
                title,
                *selected,
                filtered.len(),
                !confirmation,
                width,
            );
            let filter_line =
                (!confirmation).then(|| panel_filter_line(&state.theme, filter, width));
            if max_rows == 1 {
                return vec![if confirmation {
                    header
                } else {
                    filter_line.expect("filter row")
                }];
            }
            if max_rows == 2 && !confirmation {
                return vec![header, filter_line.expect("filter row")];
            }

            // Permission prompts are intentionally lightweight: the prompt and
            // two choices are enough context. In particular, do not surface
            // effect hashes or duplicate details beside both choices.
            let show_borders = !confirmation
                && state.theme.layout_for_width(width).show_panel_borders
                && max_rows >= 4;
            let border_rows = usize::from(show_borders) * 2;
            let chrome_rows = usize::from(!confirmation) + 1;
            let mut lines = Vec::with_capacity(max_rows);
            if show_borders {
                lines.push(dim(&rule));
            }
            lines.push(header);
            if let Some(filter_line) = filter_line {
                lines.push(filter_line);
            }
            let max_body = max_rows.saturating_sub(chrome_rows + border_rows);
            if filtered.is_empty() && max_body > 0 {
                let message = if filter.is_empty() {
                    "  No matches".to_owned()
                } else if state.theme.unicode() {
                    format!("  No matches for “{}”", panel_cell(filter))
                } else {
                    format!("  No matches for \"{}\"", panel_cell(filter))
                };
                lines.push(fit_line(&dim(&message), width));
            } else if !filtered.is_empty() {
                let visible = filtered.len().min(max_body);
                let window = panel_window(*selected, filtered.len(), visible);
                let label_width = (!confirmation)
                    .then(|| panel_label_width(items, descriptions, &filtered, width))
                    .flatten();
                for position in window {
                    let index = filtered[position];
                    lines.push(render_panel_item(
                        state,
                        &items[index],
                        (!confirmation)
                            .then(|| descriptions.get(index).and_then(|value| value.as_deref()))
                            .flatten(),
                        position == *selected,
                        label_width,
                        width,
                    ));
                }
            }
            if show_borders {
                lines.push(dim(&rule));
            }
            lines
        }
        Panel::SessionPicker { picker } => {
            render_session_picker(state, picker, width, max_rows, &rule)
        }
        Panel::MessagePicker { picker } => {
            render_message_picker(state, picker, width, max_rows, &rule)
        }
        Panel::ReadOnlyDocument {
            title,
            text,
            styled,
            scroll_from_bottom,
        } => {
            let show_borders =
                state.theme.layout_for_width(width).show_panel_borders && max_rows >= 5;
            let body_rows = document_body_rows(state, width, max_rows);
            let visual = document_visual_lines_styled(text, width, *styled);
            let maximum = visual.len().saturating_sub(body_rows);
            let scroll = (*scroll_from_bottom).min(maximum);
            let end = visual.len().saturating_sub(scroll);
            let start = end.saturating_sub(body_rows);
            let mut lines = Vec::with_capacity(max_rows);
            if show_borders {
                lines.push(dim(&rule));
            }
            lines.push(panel_header(
                &state.theme,
                title,
                0,
                visual.len(),
                false,
                width,
            ));
            lines.extend(visual[start..end].iter().map(|line| fit_line(line, width)));
            let range = if visual.is_empty() {
                "0/0".to_owned()
            } else {
                format!("{}-{}/{}", start + 1, end, visual.len())
            };
            let hint = if state.theme.unicode() {
                format!("  {range} · ↑↓ scroll · esc/← back")
            } else {
                format!("  {range} · up/down scroll · esc/left back")
            };
            lines.push(fit_line(&dim(&hint), width));
            if show_borders {
                lines.push(dim(&rule));
            }
            lines.truncate(max_rows);
            lines
        }
    }
}

#[cfg(test)]
pub mod panel_render_test_hook {
    pub fn document_lines(text: &str, width: u16, styled: bool) -> Vec<String> {
        super::document_visual_lines_styled(text, width, styled)
    }
}
