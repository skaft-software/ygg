//! Select-list panel filtering, layout, and rendering.

use sexy_tui_rs::{visible_width, wrap_text_with_ansi, CURSOR_MARKER};
use unicode_segmentation::UnicodeSegmentation;

use super::{fit_line, subdued_text, Panel, ShellState};
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

#[cfg(test)]
pub mod panel_render_test_hook {
    pub fn document_lines(text: &str, width: u16, styled: bool) -> Vec<String> {
        super::document_visual_lines_styled(text, width, styled)
    }
}

fn is_confirmation_panel(action: &super::PanelAction) -> bool {
    matches!(action, super::PanelAction::ExtensionConfirmation)
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
