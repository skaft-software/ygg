//! Select-list panel filtering, layout, and rendering.

use std::cmp::Ordering;
use std::time::{Duration, Instant, SystemTime};

use sexy_tui_rs::{visible_width, wrap_text_with_ansi, CURSOR_MARKER};
use unicode_segmentation::UnicodeSegmentation;

use super::{
    fit_line, subdued_text, ForkMessage, MessagePicker, Panel, PanelAction, PickerScope,
    PickerSort, PickerState, ShellState,
};
use crate::tui::fuzzy::{fuzzy_match, parse_search_query, SearchMode, TokenKind};
use crate::tui::layout::{PickerLayout, PresentationLayout, MAX_APPROVAL_DETAIL_ROWS};
use crate::tui::theme::YggTheme;

/// Indices of the items matching the current filter. Every whitespace-delimited
/// term must appear in either the label or description, case-insensitively.
pub(super) fn filtered_indices(
    items: &[String],
    descriptions: &[Option<String>],
    filter: &str,
) -> Vec<usize> {
    filtered_indices_with_groups(items, descriptions, None, filter)
}

fn filtered_indices_with_groups(
    items: &[String],
    descriptions: &[Option<String>],
    groups: Option<&[String]>,
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
            if let Some(group) = groups.and_then(|groups| groups.get(*index)) {
                searchable.push(' ');
                searchable.push_str(&group.to_lowercase());
            }
            needles.iter().all(|needle| searchable.contains(needle))
        })
        .map(|(index, _)| index)
        .collect()
}

pub(super) fn filtered_indices_for_action(
    items: &[String],
    descriptions: &[Option<String>],
    action: &PanelAction,
    filter: &str,
) -> Vec<usize> {
    filtered_indices_with_groups(items, descriptions, action.model_provider_groups(), filter)
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
                && meta
                    .name
                    .as_deref()
                    .is_none_or(|name| name.trim().is_empty())
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
    let terminal_width = width;
    let plan = PresentationLayout::new(&state.theme, width);
    let inset = usize::from(plan.inset);
    let width = usize::from(plan.content_width);
    let left = state.theme.bold(&picker_header_title(picker));
    let right = picker_scope_text(&state.theme, picker);
    let right = sexy_tui_rs::truncate_to_width(&right, width, Some("…"));
    let gap = width
        .saturating_sub(visible_width(&left))
        .saturating_sub(visible_width(&right))
        .max(1);
    fit_line(
        &format!(
            "{}{left}{}{}{}",
            " ".repeat(inset),
            " ".repeat(gap),
            right,
            " ".repeat(inset)
        ),
        terminal_width,
    )
}

fn picker_filter_line(state: &ShellState, picker: &PickerState, width: u16) -> String {
    let (label, value) = match picker.rename.as_deref() {
        Some(value) => ("Rename", value),
        None => ("Filter", picker.filter.as_str()),
    };
    let terminal_width = width;
    let plan = PresentationLayout::new(&state.theme, width);
    let prefix = format!(
        "{}{}  ",
        " ".repeat(usize::from(plan.inset)),
        subdued_text(&state.theme, label)
    );
    let available =
        usize::from(plan.inset + plan.content_width).saturating_sub(visible_width(&prefix));
    let ellipsis = if state.theme.unicode() { "…" } else { "..." };
    let value = panel_cell(value);
    if value.is_empty() {
        let placeholder = if label == "Rename" {
            "enter a session name"
        } else {
            "type to filter"
        };
        let placeholder = sexy_tui_rs::truncate_to_width(placeholder, available, Some(ellipsis));
        fit_line(
            &format!(
                "{prefix}{CURSOR_MARKER}{}",
                subdued_text(&state.theme, &placeholder)
            ),
            terminal_width,
        )
    } else {
        let query = sexy_tui_rs::truncate_to_width(&value, available, Some(ellipsis));
        fit_line(
            &format!(
                "{prefix}{}{CURSOR_MARKER}",
                state.theme.fg("foreground", &query)
            ),
            terminal_width,
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
        "tab scope · re:<pattern> · \"phrase\" exact".to_owned()
    };
    let second = if picker.confirming_delete || picker.rename.is_some() {
        String::new()
    } else {
        "^s sort · ^n named · del trash · ^p path on/off · ^r rename".to_owned()
    };
    let inset = " ".repeat(usize::from(
        PresentationLayout::new(&state.theme, width).inset,
    ));
    (
        fit_line(&format!("{inset}{first}"), width),
        fit_line(
            &format!("{inset}{}", subdued_text(&state.theme, &second)),
            width,
        ),
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

fn is_unreadable_session(meta: &crate::session_store::SessionMeta) -> bool {
    meta.title.eq_ignore_ascii_case("(unreadable session)")
}

fn compact_session_id(id: &str, unicode: bool) -> String {
    let characters = id.chars().collect::<Vec<_>>();
    if characters.len() <= 16 {
        return id.to_owned();
    }
    let prefix = characters[..8].iter().collect::<String>();
    let suffix = characters[characters.len().saturating_sub(6)..]
        .iter()
        .collect::<String>();
    let ellipsis = if unicode { "…" } else { "..." };
    format!("{prefix}{ellipsis}{suffix}")
}

fn session_title(
    state: &ShellState,
    meta: &crate::session_store::SessionMeta,
    is_current: bool,
) -> String {
    let mut label = String::new();
    if meta.pinned {
        label.push_str(state.theme.glyph("bullet"));
        label.push(' ');
    }
    if is_unreadable_session(meta) {
        label.push_str("(unreadable session · ");
        label.push_str(&compact_session_id(&meta.id, state.theme.unicode()));
        label.push(')');
    } else {
        label.push_str(&meta.title);
    }
    if meta.forked_from_session_id.is_some() {
        label.push_str(" (fork)");
    }
    if is_current {
        label.push_str(" (current)");
    }
    panel_cell(&label)
}

fn session_detail(
    meta: &crate::session_store::SessionMeta,
    picker: &PickerState,
    now: SystemTime,
) -> String {
    let mut details = Vec::with_capacity(5);
    if picker.scope == PickerScope::All {
        details.push(picker_workspace(meta));
    }
    details.push(format_age(meta.modified, now));
    if meta.message_count > 0 {
        let suffix = if meta.message_count == 1 {
            "msg"
        } else {
            "msgs"
        };
        details.push(format!("{} {suffix}", meta.message_count));
    }
    if is_unreadable_session(meta) {
        details.push("transcript unavailable".to_owned());
    }
    if picker.show_path {
        details.push(shorten_home_path(&meta.path));
    }
    details.join(" · ")
}

fn session_rows_are_stacked(state: &ShellState, width: u16, body_rows: usize) -> bool {
    body_rows >= 2 && PresentationLayout::new(&state.theme, width).picker == PickerLayout::Stacked
}

fn render_picker_row(
    state: &ShellState,
    meta: &crate::session_store::SessionMeta,
    picker: &PickerState,
    selected: bool,
    confirming: bool,
    stacked: bool,
    width: u16,
) -> Vec<String> {
    let is_current = picker.current_session_path.as_ref() == Some(&meta.path);
    let label = session_title(state, meta, is_current);
    let now = SystemTime::now();
    let right = panel_cell(&session_detail(meta, picker, now));
    let inset = " ".repeat(usize::from(
        PresentationLayout::new(&state.theme, width).inset,
    ));
    let cursor = if selected {
        format!("{inset}{}", state.theme.fg("accent", "› "))
    } else {
        format!("{inset}  ")
    };
    let ellipsis = if state.theme.unicode() { "…" } else { "..." };
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

    if stacked {
        let available = usize::from(width).saturating_sub(visible_width(&cursor));
        let label = sexy_tui_rs::truncate_to_width(&label, available.max(1), Some(ellipsis));
        let detail_prefix = format!("{inset}  ");
        let detail_width = usize::from(width).saturating_sub(visible_width(&detail_prefix));
        let detail = sexy_tui_rs::truncate_to_width(&right, detail_width, Some(ellipsis));
        return vec![
            fit_line(&format!("{cursor}{label}"), width),
            fit_line(
                &format!("{detail_prefix}{}", subdued_text(&state.theme, &detail)),
                width,
            ),
        ];
    }

    let right_width = visible_width(&right);
    let available = usize::from(width)
        .saturating_sub(visible_width(&cursor))
        .saturating_sub(right_width)
        .saturating_sub(1);
    let label = sexy_tui_rs::truncate_to_width(&label, available.max(1), Some(ellipsis));
    let spacing = usize::from(width)
        .saturating_sub(visible_width(&cursor))
        .saturating_sub(visible_width(&label))
        .saturating_sub(right_width)
        .max(1);
    vec![fit_line(
        &format!(
            "{cursor}{label}{}{}",
            " ".repeat(spacing),
            subdued_text(&state.theme, &right)
        ),
        width,
    )]
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
                "No named sessions in current workspace. Press ^n to show all, or tab to view all."
                    .to_owned()
            }
            PickerScope::All => "No named sessions found. Press ^n to show all.".to_owned(),
        }
    } else {
        match picker.scope {
            PickerScope::Current => {
                "No sessions in current workspace. Press tab to view all.".to_owned()
            }
            PickerScope::All => "No sessions found".to_owned(),
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
    let inset = " ".repeat(usize::from(
        PresentationLayout::new(&state.theme, width).inset,
    ));
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
                &format!(
                    "{inset}{}",
                    subdued_text(&state.theme, "Loading all workspaces…")
                ),
                width,
            ));
        } else if ordering.is_empty() {
            lines.push(fit_line(
                &format!(
                    "{inset}{}",
                    subdued_text(&state.theme, &session_empty_message(picker))
                ),
                width,
            ));
        } else {
            // Wide pickers use a title line plus a dim metadata line. This
            // keeps timestamps/counts from competing with a long prompt and
            // gives the title the full terminal width before truncation.
            let stacked = session_rows_are_stacked(state, width, body_rows);
            let row_height = usize::from(stacked) + 1;
            let show_indicator = ordering.len() > body_rows / row_height
                && body_rows >= row_height.saturating_add(1);
            let visible_rows = body_rows.saturating_sub(usize::from(show_indicator));
            let visible_items = (visible_rows / row_height).min(ordering.len());
            let window = panel_window(picker.selected, ordering.len(), visible_items);
            for position in window {
                if let Some(meta) = picker.active_rows().get(ordering[position]) {
                    lines.extend(render_picker_row(
                        state,
                        meta,
                        picker,
                        position == picker.selected,
                        picker.confirming_delete && position == picker.selected,
                        stacked,
                        width,
                    ));
                }
            }
            if show_indicator {
                let selected = picker.selected.min(ordering.len().saturating_sub(1));
                lines.push(fit_line(
                    &subdued_text(
                        &state.theme,
                        &format!("{inset}({}/{})", selected + 1, ordering.len()),
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

pub(super) fn document_content_width(theme: &YggTheme, width: u16) -> u16 {
    PresentationLayout::new(theme, width).content_width
}

/// `styled` text was already sanitized at its producing boundary and carries
/// trusted theme ANSI that must survive wrapping.
pub(super) fn document_visual_lines_styled(
    text: &str,
    theme: &YggTheme,
    width: u16,
    styled: bool,
) -> Vec<String> {
    let plan = PresentationLayout::new(theme, width);
    let inset = usize::from(plan.inset);
    let available = usize::from(plan.content_width);
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

pub(super) fn document_visual_row_count_styled(
    text: &str,
    theme: &YggTheme,
    width: u16,
    styled: bool,
) -> usize {
    document_visual_lines_styled(text, theme, width, styled).len()
}

fn is_confirmation_panel(action: &super::PanelAction) -> bool {
    matches!(action, super::PanelAction::Confirmation)
}

fn confirmation_detail(descriptions: &[Option<String>]) -> Option<&str> {
    descriptions
        .iter()
        .find_map(|description| description.as_deref())
        .map(str::trim)
        .filter(|detail| !detail.is_empty())
}

fn bounded_confirmation_detail(detail: &str) -> String {
    const MAX_DETAIL_BYTES: usize = 4 * 1024;
    let mut detail = panel_cell(detail);
    if detail.len() <= MAX_DETAIL_BYTES {
        return detail;
    }
    let mut end = MAX_DETAIL_BYTES.saturating_sub('…'.len_utf8());
    while end > 0 && !detail.is_char_boundary(end) {
        end -= 1;
    }
    detail.truncate(end);
    detail.push('…');
    detail
}

fn append_confirmation_omission_marker(line: &mut String, content_width: usize, unicode: bool) {
    let marker = sexy_tui_rs::strip_terminal_sequences(&sexy_tui_rs::truncate_to_width(
        if unicode { "…" } else { "..." },
        content_width,
        Some(""),
    ));
    if marker.is_empty() {
        return;
    }
    let separator = usize::from(content_width > visible_width(&marker));
    let retained_width = content_width
        .saturating_sub(visible_width(&marker))
        .saturating_sub(separator);
    let retained = sexy_tui_rs::strip_terminal_sequences(&sexy_tui_rs::truncate_to_width(
        line,
        retained_width,
        Some(""),
    ));
    *line = format!(
        "{}{}{}",
        retained.trim_end(),
        if separator == 1 { " " } else { "" },
        marker
    );
}

fn confirmation_detail_lines(
    state: &ShellState,
    descriptions: &[Option<String>],
    width: u16,
    available_rows: usize,
) -> Vec<String> {
    let Some(detail) = confirmation_detail(descriptions) else {
        return Vec::new();
    };
    let plan = PresentationLayout::new(&state.theme, width);
    let terminal_width = usize::from(width);
    let inset = usize::from(plan.inset).min(terminal_width);
    let label = "Detail";
    let full_plain_prefix = format!("{}{label}  ", " ".repeat(inset));
    let show_label = visible_width(&full_plain_prefix).saturating_add(inset) < terminal_width;
    let plain_prefix = if show_label {
        full_plain_prefix
    } else {
        " ".repeat(inset.min(terminal_width.saturating_sub(1)))
    };
    let continuation = " ".repeat(visible_width(&plain_prefix));
    let content_width = terminal_width
        .saturating_sub(visible_width(&plain_prefix) + inset)
        .max(1);
    let detail = bounded_confirmation_detail(detail);
    let mut wrapped = wrap_text_with_ansi(&detail, content_width);
    let rendered_rows = available_rows.min(MAX_APPROVAL_DETAIL_ROWS);
    let omitted = wrapped.len() > rendered_rows;
    wrapped.truncate(rendered_rows);
    if omitted {
        if let Some(last) = wrapped.last_mut() {
            append_confirmation_omission_marker(last, content_width, state.theme.unicode());
        }
    }
    wrapped
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 && show_label {
                format!(
                    "{}{}  ",
                    " ".repeat(inset),
                    state.theme.fg("warning", label)
                )
            } else {
                continuation.clone()
            };
            fit_line(
                &format!("{prefix}{}", subdued_text(&state.theme, &line)),
                width,
            )
        })
        .collect()
}

struct PanelHeaderRender {
    line: String,
}

fn panel_header(
    theme: &YggTheme,
    title: &str,
    selected: usize,
    matches: usize,
    show_position: bool,
    width: u16,
) -> PanelHeaderRender {
    let terminal_width = width;
    let plan = PresentationLayout::new(theme, width);
    let width = usize::from(width);
    let inset = usize::from(plan.inset);
    let available = usize::from(plan.content_width);
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
    PanelHeaderRender {
        line: fit_line(&line, terminal_width),
    }
}

fn panel_filter_line(theme: &YggTheme, filter: &str, width: u16) -> String {
    let terminal_width = width;
    let plan = PresentationLayout::new(theme, width);
    let width = usize::from(plan.content_width);
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
        format!("{}{label}  ", " ".repeat(usize::from(plan.inset)))
    };
    let available =
        usize::from(plan.inset + plan.content_width).saturating_sub(visible_width(&prefix));
    let filter = panel_cell(filter);
    if filter.is_empty() {
        let placeholder = sexy_tui_rs::truncate_to_width(
            "type to filter",
            available,
            Some(if theme.unicode() { "…" } else { "..." }),
        );
        fit_line(
            &format!(
                "{prefix}{CURSOR_MARKER}{}",
                subdued_text(theme, &placeholder)
            ),
            terminal_width,
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
        fit_line(
            &format!("{prefix}{}{CURSOR_MARKER}", theme.fg("foreground", &query)),
            terminal_width,
        )
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

fn provider_heading_count(
    filtered: &[usize],
    providers: Option<&[String]>,
    range: std::ops::Range<usize>,
) -> usize {
    let Some(providers) = providers else {
        return 0;
    };
    let mut previous: Option<&str> = None;
    let mut count = 0usize;
    for position in range {
        let Some(provider) = filtered
            .get(position)
            .and_then(|index| providers.get(*index))
            .map(String::as_str)
        else {
            continue;
        };
        if previous != Some(provider) {
            count = count.saturating_add(1);
            previous = Some(provider);
        }
    }
    count
}

fn grouped_panel_window(
    selected: usize,
    filtered: &[usize],
    providers: Option<&[String]>,
    row_height: usize,
    max_rows: usize,
) -> std::ops::Range<usize> {
    if filtered.is_empty() || row_height == 0 || max_rows < row_height {
        return 0..0;
    }
    let visible = (max_rows / row_height).max(1).min(filtered.len());
    let mut range = panel_window(selected, filtered.len(), visible);
    while range.start < range.end
        && range
            .len()
            .saturating_mul(row_height)
            .saturating_add(provider_heading_count(filtered, providers, range.clone()))
            > max_rows
    {
        if range.len() == 1 {
            break;
        }
        let selected = selected.min(filtered.len().saturating_sub(1));
        let left = selected.saturating_sub(range.start);
        let right = range.end.saturating_sub(selected.saturating_add(1));
        if right >= left && range.end > selected.saturating_add(1) {
            range.end = range.end.saturating_sub(1);
        } else if range.start < selected {
            range.start = range.start.saturating_add(1);
        } else {
            range.end = range.end.saturating_sub(1);
        }
    }
    range
}

fn render_provider_heading(state: &ShellState, provider: &str, width: u16) -> String {
    let plan = PresentationLayout::new(&state.theme, width);
    let prefix = format!("{}  ", " ".repeat(usize::from(plan.inset)));
    let available = usize::from(width)
        .saturating_sub(visible_width(&prefix))
        .saturating_sub(usize::from(plan.inset));
    let provider = sexy_tui_rs::truncate_to_width(
        &panel_cell(provider),
        available,
        Some(if state.theme.unicode() { "…" } else { "..." }),
    );
    fit_line(&format!("{prefix}{}", state.theme.bold(&provider)), width)
}

fn select_list_uses_stacked_rows(
    state: &ShellState,
    action: &PanelAction,
    descriptions: &[Option<String>],
    width: u16,
    available_rows: usize,
) -> bool {
    action.is_model_picker()
        && PresentationLayout::new(&state.theme, width).picker == PickerLayout::Stacked
        && available_rows
            >= if action.model_provider_groups().is_some() {
                3
            } else {
                2
            }
        && descriptions.iter().any(|description| {
            description
                .as_deref()
                .is_some_and(|description| !description.is_empty())
        })
}

fn panel_label_width(
    state: &ShellState,
    items: &[String],
    descriptions: &[Option<String>],
    filtered: &[usize],
    width: u16,
) -> Option<usize> {
    let content_width = usize::from(PresentationLayout::new(&state.theme, width).content_width);
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

struct PanelItemRender {
    lines: Vec<String>,
    sanitized_label: String,
    label_complete: bool,
}

fn render_panel_item(
    state: &ShellState,
    item: &str,
    description: Option<&str>,
    is_selected: bool,
    label_width: Option<usize>,
    stacked: bool,
    width: u16,
) -> PanelItemRender {
    let item = panel_cell(item);
    let marker = state.theme.glyph("prompt");
    let inset = " ".repeat(usize::from(
        PresentationLayout::new(&state.theme, width).inset,
    ));
    let prefix = if is_selected {
        // Picker focus is UI state, not the currently selected model's lab
        // colour. The global accent stays legible even when a provider's
        // brand colour is intentionally subdued (for example OpenAI black).
        format!("{inset}{} ", state.theme.fg("accent", marker))
    } else {
        format!("{inset}  ")
    };
    let available = usize::from(width).saturating_sub(visible_width(&prefix));
    let ellipsis = if state.theme.unicode() { "…" } else { "..." };

    if stacked {
        let label = sexy_tui_rs::truncate_to_width(&item, available, Some(ellipsis));
        let label = if is_selected {
            state.theme.bold(&state.theme.fg("accent", &label))
        } else {
            label
        };
        let mut lines = vec![fit_line(&format!("{prefix}{label}"), width)];
        let detail_prefix = format!("{inset}  ");
        let detail_width = usize::from(width).saturating_sub(visible_width(&detail_prefix));
        if let Some(description) = description {
            let description = sexy_tui_rs::truncate_to_width(
                &panel_cell(description),
                detail_width,
                Some(ellipsis),
            );
            lines.push(fit_line(
                &format!(
                    "{detail_prefix}{}",
                    subdued_text(&state.theme, &description)
                ),
                width,
            ));
        } else {
            // Keep every item in a model list on the same two-row rhythm.
            lines.push(String::new());
        }
        return PanelItemRender {
            lines,
            label_complete: !item.trim().is_empty() && visible_width(&item) <= available,
            sanitized_label: item,
        };
    }

    let rendered_label_width = label_width.unwrap_or(available);
    let label = if let Some(label_width) = label_width {
        sexy_tui_rs::truncate_to_width(&item, label_width, Some(ellipsis))
    } else {
        sexy_tui_rs::truncate_to_width(&item, available, Some(ellipsis))
    };
    let label = if is_selected {
        state.theme.bold(&state.theme.fg("accent", &label))
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
    PanelItemRender {
        lines: vec![fit_line(&line, width)],
        label_complete: !item.trim().is_empty() && visible_width(&item) <= rendered_label_width,
        sanitized_label: item,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConfirmationRenderMetadata {
    selected_action: Option<RenderedConfirmationAction>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RenderedConfirmationAction {
    item_index: usize,
    sanitized_label: String,
    label_complete: bool,
}

pub(super) fn confirmation_enter_allowed(
    rendered: Option<&ConfirmationRenderMetadata>,
    item_index: usize,
    item_label: &str,
) -> bool {
    let sanitized_label = panel_cell(item_label);
    rendered.is_some_and(|rendered| {
        rendered.selected_action.as_ref().is_some_and(|action| {
            action.item_index == item_index
                && action.sanitized_label == sanitized_label
                && action.label_complete
        })
    })
}

struct PanelRenderOutput {
    lines: Vec<String>,
    confirmation: Option<ConfirmationRenderMetadata>,
}

impl PanelRenderOutput {
    fn lines(lines: Vec<String>) -> Self {
        Self {
            lines,
            confirmation: None,
        }
    }
}

/// How many rows the active panel needs (capped so it cannot squeeze the
/// transcript to zero).
#[cfg(test)]
fn panel_rows(state: &ShellState, width: u16) -> usize {
    let Some(ref panel) = state.panel else {
        return 0;
    };
    let max_panel = usize::from(state.size.1.max(5)).saturating_sub(4);
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
            let filtered = filtered_indices_for_action(items, descriptions, action, filter);
            let body = filtered.len().max(1);
            let border_rows = usize::from(
                !confirmation
                    && state.theme.layout_for_width(width).show_panel_borders
                    && max_panel >= 4,
            ) * 2;
            let detail_rows = if confirmation {
                let available_detail_rows =
                    max_panel.saturating_sub(1 + usize::from(!filtered.is_empty()));
                confirmation_detail_lines(state, descriptions, width, available_detail_rows).len()
            } else {
                0
            };
            let chrome_rows = if confirmation { 1 + detail_rows } else { 2 };
            let available_rows = max_panel.saturating_sub(chrome_rows + border_rows);
            let row_height = usize::from(select_list_uses_stacked_rows(
                state,
                action,
                descriptions,
                width,
                available_rows,
            )) + 1;
            let headings = provider_heading_count(
                &filtered,
                action.model_provider_groups(),
                0..filtered.len(),
            );
            (body * row_height + headings + chrome_rows + border_rows).min(max_panel)
        }
        Panel::SessionPicker { picker } => {
            let body = session_picker_ordering(picker).len().max(1);
            let border_rows = usize::from(
                state.theme.layout_for_width(width).show_panel_borders && max_panel >= 6,
            ) * 2;
            let available_rows = max_panel.saturating_sub(4 + border_rows);
            let row_height =
                usize::from(session_rows_are_stacked(state, width, available_rows)) + 1;
            (body * row_height + 4 + border_rows).min(max_panel)
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
            (document_visual_row_count_styled(text, &state.theme, width, *styled) + 2 + border_rows)
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
    render_panel_output_with_limit(state, width, max_rows).lines
}

fn render_panel_output_with_limit(
    state: &ShellState,
    width: u16,
    max_rows: usize,
) -> PanelRenderOutput {
    let Some(ref panel) = state.panel else {
        return PanelRenderOutput::lines(Vec::new());
    };
    if max_rows == 0 {
        return PanelRenderOutput::lines(Vec::new());
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
            let filtered = filtered_indices_for_action(items, descriptions, action, filter);
            let header = panel_header(
                &state.theme,
                title,
                *selected,
                filtered.len(),
                !confirmation,
                width,
            );
            let mut confirmation_metadata = confirmation.then_some(ConfirmationRenderMetadata {
                selected_action: None,
            });
            let header = header.line;
            let filter_line =
                (!confirmation).then(|| panel_filter_line(&state.theme, filter, width));
            if max_rows == 1 {
                return PanelRenderOutput {
                    lines: vec![if confirmation {
                        header
                    } else {
                        filter_line.expect("filter row")
                    }],
                    confirmation: confirmation_metadata,
                };
            }
            if max_rows == 2 && !confirmation {
                return PanelRenderOutput::lines(vec![header, filter_line.expect("filter row")]);
            }

            // Approval detail is shared evidence rendered once above the
            // actions. At constrained heights, always reserve a row for the
            // selected action so Enter can never confirm invisible UI.
            let show_borders = !confirmation
                && state.theme.layout_for_width(width).show_panel_borders
                && max_rows >= 4;
            let mut lines = Vec::with_capacity(max_rows);
            if show_borders {
                lines.push(dim(&rule));
            }
            lines.push(header);
            if let Some(filter_line) = filter_line {
                lines.push(filter_line);
            }
            if confirmation {
                let action_rows = usize::from(!filtered.is_empty());
                let available_detail_rows = max_rows.saturating_sub(lines.len() + action_rows);
                lines.extend(confirmation_detail_lines(
                    state,
                    descriptions,
                    width,
                    available_detail_rows,
                ));
            }
            let max_body = max_rows.saturating_sub(lines.len() + usize::from(show_borders));
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
                let stacked =
                    select_list_uses_stacked_rows(state, action, descriptions, width, max_body);
                let row_height = usize::from(stacked) + 1;
                // A provider heading is presentation metadata, not an item. It
                // consumes a row but never participates in keyboard selection.
                // At a one-row emergency height, retain the selected model and
                // omit only its heading so Enter cannot confirm invisible UI.
                let providers = action
                    .model_provider_groups()
                    .filter(|_| max_body >= row_height.saturating_add(1));
                let window =
                    grouped_panel_window(*selected, &filtered, providers, row_height, max_body);
                let label_width = (!confirmation && !stacked)
                    .then(|| panel_label_width(state, items, descriptions, &filtered, width))
                    .flatten();
                let mut previous_provider: Option<&str> = None;
                for position in window {
                    let index = filtered[position];
                    if let Some(provider) = providers
                        .and_then(|providers| providers.get(index))
                        .map(String::as_str)
                    {
                        if previous_provider != Some(provider) {
                            lines.push(render_provider_heading(state, provider, width));
                            previous_provider = Some(provider);
                        }
                    }
                    let is_selected = position == *selected;
                    let item_render = render_panel_item(
                        state,
                        &items[index],
                        (!confirmation)
                            .then(|| descriptions.get(index).and_then(|value| value.as_deref()))
                            .flatten(),
                        is_selected,
                        label_width,
                        stacked,
                        width,
                    );
                    if confirmation && is_selected {
                        if let Some(metadata) = confirmation_metadata.as_mut() {
                            metadata.selected_action = Some(RenderedConfirmationAction {
                                item_index: index,
                                sanitized_label: item_render.sanitized_label.clone(),
                                label_complete: item_render.label_complete,
                            });
                        }
                    }
                    lines.extend(item_render.lines);
                }
            }
            if show_borders {
                lines.push(dim(&rule));
            }
            PanelRenderOutput {
                lines,
                confirmation: confirmation_metadata,
            }
        }
        Panel::SessionPicker { picker } => {
            PanelRenderOutput::lines(render_session_picker(state, picker, width, max_rows, &rule))
        }
        Panel::MessagePicker { picker } => {
            PanelRenderOutput::lines(render_message_picker(state, picker, width, max_rows, &rule))
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
            let visual = document_visual_lines_styled(text, &state.theme, width, *styled);
            let maximum = visual.len().saturating_sub(body_rows);
            let scroll = (*scroll_from_bottom).min(maximum);
            let end = visual.len().saturating_sub(scroll);
            let start = end.saturating_sub(body_rows);
            let mut lines = Vec::with_capacity(max_rows);
            if show_borders {
                lines.push(dim(&rule));
            }
            lines.push(panel_header(&state.theme, title, 0, visual.len(), false, width).line);
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
            PanelRenderOutput::lines(lines)
        }
    }
}

pub(super) fn confirmation_metadata_for_rendered_panel(
    state: &ShellState,
    width: u16,
    rendered_lines: &[String],
) -> Option<ConfirmationRenderMetadata> {
    let rendered = render_panel_output_with_limit(state, width, rendered_lines.len());
    if rendered.lines.as_slice() != rendered_lines {
        return None;
    }
    rendered.confirmation
}

#[cfg(test)]
mod grouped_model_tests {
    use super::*;

    fn action() -> PanelAction {
        PanelAction::SelectGroupedModel {
            models: vec![
                ygg_ai::ModelId("a".into()),
                ygg_ai::ModelId("b".into()),
                ygg_ai::ModelId("c".into()),
            ],
            providers: vec!["Anthropic".into(), "OpenAI".into(), "OpenAI".into()],
        }
    }

    #[test]
    fn provider_headings_are_search_metadata_not_selectable_items() {
        let items = vec!["Claude".into(), "GPT 4o".into(), "GPT 5".into()];
        let descriptions = vec![
            Some("in $3/M  out $15/M  200k ctx  vision".into()),
            Some("in $5/M  out $20/M  128k ctx  vision".into()),
            Some("in $2/M  out $10/M  400k ctx  vision".into()),
        ];
        let action = action();
        let filtered = filtered_indices_for_action(&items, &descriptions, &action, "openai");
        assert_eq!(filtered, vec![1, 2]);
        assert_eq!(
            provider_heading_count(&filtered, action.model_provider_groups(), 0..filtered.len()),
            1
        );
    }

    #[test]
    fn grouped_window_reserves_heading_rows_without_hiding_selection() {
        let filtered = vec![0, 1, 2];
        let action = action();
        let window = grouped_panel_window(1, &filtered, action.model_provider_groups(), 1, 3);
        assert!(
            window.contains(&1),
            "selected model left the visible window"
        );
        assert!(
            window.len()
                + provider_heading_count(&filtered, action.model_provider_groups(), window.clone())
                <= 3
        );
    }

    #[test]
    fn grouped_model_panel_renders_each_provider_once_without_selecting_headings() {
        let mut shell = super::super::InteractiveShell::test_shell();
        shell.set_size(120, 24);
        shell.open_panel(Panel::SelectList {
            title: "Select model".into(),
            items: vec!["Claude".into(), "GPT 4o".into(), "GPT 5".into()],
            descriptions: vec![
                Some("in $3/M  out $15/M  200k ctx  vision".into()),
                Some("in $5/M  out $20/M  128k ctx  vision".into()),
                Some("in $2/M  out $10/M  400k ctx  vision".into()),
            ],
            selected: 0,
            filter: String::new(),
            action: action(),
        });
        let state = shell.state.borrow();
        let plain = render_panel_with_limit(&state, 120, 16)
            .into_iter()
            .map(|line| sexy_tui_rs::strip_terminal_sequences(&line))
            .collect::<Vec<_>>();
        assert_eq!(
            plain
                .iter()
                .filter(|line| line.trim() == "Anthropic")
                .count(),
            1,
            "{plain:?}"
        );
        assert_eq!(
            plain.iter().filter(|line| line.trim() == "OpenAI").count(),
            1,
            "{plain:?}"
        );
        assert!(
            plain.iter().any(|line| line.contains("› Claude")),
            "{plain:?}"
        );
        assert!(
            plain
                .iter()
                .filter(|line| line.trim() == "Anthropic" || line.trim() == "OpenAI")
                .all(|line| !line.contains('›')),
            "{plain:?}"
        );
    }
}

#[cfg(test)]
pub mod panel_render_test_hook {
    pub fn document_lines(text: &str, width: u16, styled: bool) -> Vec<String> {
        let theme = crate::tui::theme::test_theme();
        super::document_visual_lines_styled(text, &theme, width, styled)
    }
}
