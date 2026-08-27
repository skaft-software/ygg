use sexy_tui_rs::{strip_terminal_sequences, visible_width};

use crate::tui::theme::{ThemeSurfaceChrome, ThemeSurfaceHeading, YggTheme};

use super::surface_layout::{surface_roles, SurfacePlan};
use super::{fit_line, TranscriptBlock};

fn padded_to_width(line: &str, width: u16) -> String {
    let line = fit_line(line, width);
    let padding = usize::from(width).saturating_sub(visible_width(&line));
    if padding == 0 {
        line
    } else {
        format!("{line}{}", " ".repeat(padding))
    }
}

fn horizontal_rule(theme: &YggTheme, width: usize) -> String {
    theme.glyph("horizontal").repeat(width)
}

fn styled_surface_heading(plan: &SurfacePlan<'_>, theme: &YggTheme) -> String {
    let (_, border_role, label_role) = surface_roles(plan.kind);
    let frame_width = usize::from(plan.frame_width);
    let left = theme.glyph("top_left");
    let right = theme.glyph("top_right");
    let label = plan.label.unwrap_or("");
    let styled_label = theme.apply_semantic_role(label_role, label);

    let raw = if plan.chrome == ThemeSurfaceChrome::Card {
        let middle_width = frame_width.saturating_sub(2);
        if label.is_empty() || plan.heading == ThemeSurfaceHeading::None {
            format!("{left}{}{right}", horizontal_rule(theme, middle_width))
        } else {
            let label_width = visible_width(label).min(middle_width.saturating_sub(2));
            let rest = middle_width.saturating_sub(label_width.saturating_add(2));
            match plan.heading {
                ThemeSurfaceHeading::Inline => format!(
                    "{left}{styled_label} {}{right}",
                    horizontal_rule(theme, middle_width.saturating_sub(label_width + 1))
                ),
                ThemeSurfaceHeading::Tab => format!(
                    "{left} {styled_label} {}{right}",
                    horizontal_rule(theme, rest)
                ),
                ThemeSurfaceHeading::Overline => format!(
                    "{left}{} {styled_label} {right}",
                    horizontal_rule(theme, rest)
                ),
                ThemeSurfaceHeading::None => unreachable!("handled above"),
            }
        }
    } else if plan.chrome == ThemeSurfaceChrome::Rule
        || plan.heading == ThemeSurfaceHeading::Overline
    {
        if label.is_empty() || plan.heading == ThemeSurfaceHeading::None {
            horizontal_rule(theme, frame_width)
        } else {
            let used = visible_width(label).saturating_add(1).min(frame_width);
            format!(
                "{styled_label} {}",
                horizontal_rule(theme, frame_width - used)
            )
        }
    } else if plan.heading == ThemeSurfaceHeading::Tab {
        let label_width = visible_width(label).min(frame_width.saturating_sub(4));
        let tail = frame_width.saturating_sub(label_width.saturating_add(4));
        format!(
            "{left} {styled_label} {}{right}",
            horizontal_rule(theme, tail)
        )
    } else {
        styled_label
    };
    theme.apply_semantic_role_layered(border_role, &padded_to_width(&raw, plan.frame_width))
}

fn render_surface_content_line(
    line: &str,
    plan: &SurfacePlan<'_>,
    theme: &YggTheme,
    prompt_color: Option<&str>,
    _collapsed_reasoning: bool,
) -> String {
    let (content_role, border_role, _) = surface_roles(plan.kind);
    let content = fit_line(line, plan.geometry.content_width);
    let left_padding = " ".repeat(usize::from(plan.padding));
    let right_padding = " ".repeat(usize::from(plan.padding));
    let paint_prompt = |text: String, width: u16| {
        let text = padded_to_width(&strip_terminal_sequences(&text), width);
        theme.prompt_color_cell(prompt_color, &text)
    };
    match plan.chrome {
        ThemeSurfaceChrome::Card => {
            let inner_width = plan.frame_width.saturating_sub(2);
            let inner = padded_to_width(
                &format!("{left_padding}{content}{right_padding}"),
                inner_width,
            );
            let inner = if prompt_color.is_some() {
                paint_prompt(inner, inner_width)
            } else {
                theme.apply_semantic_role_layered(content_role, &inner)
            };
            format!(
                "{}{}{}",
                theme.apply_semantic_role(border_role, theme.glyph("vertical")),
                inner,
                theme.apply_semantic_role(border_role, theme.glyph("vertical")),
            )
        }
        ThemeSurfaceChrome::Band => {
            let inner = padded_to_width(
                &format!("{left_padding}{content}{right_padding}"),
                plan.frame_width,
            );
            if prompt_color.is_some() {
                paint_prompt(inner, plan.frame_width)
            } else {
                theme.apply_semantic_role_layered(content_role, &inner)
            }
        }
        ThemeSurfaceChrome::Rail => {
            let rail = theme.apply_semantic_role(border_role, theme.glyph("rail"));
            let body = format!(" {left_padding}{content}{right_padding}");
            let body = if prompt_color.is_some() {
                let rail_width = u16::try_from(visible_width(theme.glyph("rail")))
                    .unwrap_or(u16::MAX)
                    .min(plan.frame_width);
                paint_prompt(body, plan.frame_width.saturating_sub(rail_width))
            } else {
                theme.apply_semantic_role_layered(content_role, &body)
            };
            fit_line(&format!("{rail}{body}"), plan.frame_width)
        }
        ThemeSurfaceChrome::Plain | ThemeSurfaceChrome::Rule => {
            let body = format!("{left_padding}{content}{right_padding}");
            if prompt_color.is_some() {
                paint_prompt(body, plan.frame_width)
            } else {
                theme.apply_semantic_role_layered(content_role, &body)
            }
        }
    }
}

fn surface_bottom_row(plan: &SurfacePlan<'_>, theme: &YggTheme) -> Option<String> {
    (plan.chrome == ThemeSurfaceChrome::Card).then(|| {
        let (_, border_role, _) = surface_roles(plan.kind);
        let middle = horizontal_rule(theme, usize::from(plan.frame_width.saturating_sub(2)));
        let bottom = format!(
            "{}{}{}",
            theme.glyph("bottom_left"),
            middle,
            theme.glyph("bottom_right")
        );
        theme.apply_semantic_role_layered(border_role, &bottom)
    })
}

#[cfg(test)]
pub(super) fn event_margin_marker(
    block: &TranscriptBlock,
    theme: &YggTheme,
    active_dot_visible: bool,
    collapsed_reasoning: bool,
) -> Option<String> {
    event_margin_marker_with_frame(
        block,
        theme,
        usize::from(!active_dot_visible),
        collapsed_reasoning,
    )
}

pub(super) fn event_margin_marker_with_frame(
    block: &TranscriptBlock,
    theme: &YggTheme,
    spinner_frame: usize,
    collapsed_reasoning: bool,
) -> Option<String> {
    let markers_enabled = theme.resolve::<bool>("margin_markers").unwrap_or(true);
    let thinking_spinner = theme.resolve::<bool>("thinking_spinner").unwrap_or(false);
    if !markers_enabled && !matches!(block, TranscriptBlock::Reasoning(_)) {
        return None;
    }
    let event_dot = if theme.unicode() { "•" } else { "*" };
    let active_dot_visible = spinner_frame % 2 == 0;
    let active_phase_dot = || {
        if active_dot_visible {
            theme.fg("foreground", event_dot)
        } else {
            theme.settled_event_dot("neutral", event_dot)
        }
    };
    match block {
        TranscriptBlock::Reasoning(_) if collapsed_reasoning && thinking_spinner => {
            const BRAILLE_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            const ASCII_FRAMES: [&str; 10] = [".", ":", "*", "+", "x", "X", "+", "*", ":", "."];
            let spinner = if theme.unicode() {
                BRAILLE_FRAMES[spinner_frame % BRAILLE_FRAMES.len()]
            } else {
                ASCII_FRAMES[spinner_frame % ASCII_FRAMES.len()]
            };
            Some(theme.fg("accent", spinner))
        }
        TranscriptBlock::Reasoning(reasoning) if collapsed_reasoning && markers_enabled => {
            Some(if active_dot_visible {
                theme.model_fg(reasoning.model_lab, event_dot)
            } else {
                theme.settled_event_dot("neutral", event_dot)
            })
        }
        TranscriptBlock::Reasoning(_) => None,
        TranscriptBlock::Assistant(_) if markers_enabled => Some(theme.fg("foreground", event_dot)),
        TranscriptBlock::Tool(panel) if markers_enabled && !panel.finished => {
            Some(active_phase_dot())
        }
        TranscriptBlock::Tool(panel) if markers_enabled => Some(if panel.is_error {
            theme.settled_event_dot("error", event_dot)
        } else {
            theme.settled_event_dot("success", event_dot)
        }),
        TranscriptBlock::Shell(shell) if markers_enabled && shell.running => {
            Some(active_phase_dot())
        }
        TranscriptBlock::Shell(shell) if markers_enabled => Some(if shell.exit_code == 0 {
            theme.settled_event_dot("success", event_dot)
        } else {
            theme.settled_event_dot("error", event_dot)
        }),
        TranscriptBlock::Notice(_) if markers_enabled => {
            Some(theme.settled_event_dot("neutral", event_dot))
        }
        TranscriptBlock::NoticeStatus { tone, .. } if markers_enabled => {
            Some(theme.settled_event_dot(
                match tone {
                    super::NoticeTone::Success => "success",
                    super::NoticeTone::Error => "error",
                },
                event_dot,
            ))
        }
        TranscriptBlock::User { .. }
        | TranscriptBlock::Outcome(_)
        | TranscriptBlock::Compaction(_)
        | TranscriptBlock::Tool(_)
        | TranscriptBlock::Shell(_)
        | TranscriptBlock::Assistant(_)
        | TranscriptBlock::Notice(_)
        | TranscriptBlock::NoticeStatus { .. } => None,
    }
}

pub(super) fn decorate_surface_with_frame(
    content: Vec<String>,
    plan: &SurfacePlan<'_>,
    theme: &YggTheme,
    outer_width: u16,
    prompt_color: Option<&str>,
    collapsed_reasoning: bool,
    marker: Option<String>,
) -> Vec<String> {
    let mut rows = Vec::with_capacity(
        plan.geometry.transition_rows
            + plan.geometry.leading_rows
            + content.len()
            + plan.geometry.trailing_rows,
    );
    let has_heading_row = plan.chrome == ThemeSurfaceChrome::Card
        || plan.chrome == ThemeSurfaceChrome::Rule
        || plan.heading != ThemeSurfaceHeading::None;
    let has_bottom_row = plan.chrome == ThemeSurfaceChrome::Card;
    let leading_padding_rows = plan.geometry.leading_rows - usize::from(has_heading_row);
    let trailing_padding_rows = plan.geometry.trailing_rows - usize::from(has_bottom_row);

    rows.extend(std::iter::repeat_n(
        String::new(),
        plan.geometry.transition_rows,
    ));
    if has_heading_row {
        rows.push(styled_surface_heading(plan, theme));
    }
    rows.extend(std::iter::repeat_n(
        render_surface_content_line("", plan, theme, prompt_color, collapsed_reasoning),
        leading_padding_rows,
    ));
    rows.extend(content.iter().map(|line| {
        render_surface_content_line(line, plan, theme, prompt_color, collapsed_reasoning)
    }));
    rows.extend(std::iter::repeat_n(
        render_surface_content_line("", plan, theme, prompt_color, collapsed_reasoning),
        trailing_padding_rows,
    ));
    if let Some(bottom) = surface_bottom_row(plan, theme) {
        rows.push(bottom);
    }

    let mut marker_pending = true;
    rows.into_iter()
        .enumerate()
        .map(|(row, line)| {
            if row < plan.geometry.transition_rows || line.is_empty() {
                String::new()
            } else {
                let frame_left = usize::from(plan.frame_left);
                let prefix = if marker_pending && marker.is_some() {
                    marker_pending = false;
                    let marker = marker.as_deref().expect("checked above");
                    if frame_left >= 2 {
                        format!("{}{marker} ", " ".repeat(frame_left - 2))
                    } else if frame_left == 1 {
                        marker.to_owned()
                    } else {
                        format!("{marker} ")
                    }
                } else {
                    " ".repeat(frame_left)
                };
                fit_line(&format!("{prefix}{line}"), outer_width)
            }
        })
        .collect()
}

/// Decorate a replacement that starts after at least one stable content row.
/// The event marker and leading frame rows are already retained in the stable
/// prefix, so this returns only content-tail and trailing-frame rows.
pub(super) fn decorate_surface_content_suffix(
    content_tail: Vec<String>,
    plan: &SurfacePlan<'_>,
    theme: &YggTheme,
    outer_width: u16,
    prompt_color: Option<&str>,
    collapsed_reasoning: bool,
) -> Vec<String> {
    let has_bottom_row = plan.chrome == ThemeSurfaceChrome::Card;
    let trailing_padding_rows = plan.geometry.trailing_rows - usize::from(has_bottom_row);
    let frame_left = " ".repeat(usize::from(plan.frame_left));
    let mut rows = content_tail
        .iter()
        .map(|line| {
            render_surface_content_line(line, plan, theme, prompt_color, collapsed_reasoning)
        })
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                fit_line(&format!("{frame_left}{line}"), outer_width)
            }
        })
        .collect::<Vec<_>>();
    rows.extend(
        std::iter::repeat_with(|| {
            let line =
                render_surface_content_line("", plan, theme, prompt_color, collapsed_reasoning);
            if line.is_empty() {
                String::new()
            } else {
                fit_line(&format!("{frame_left}{line}"), outer_width)
            }
        })
        .take(trailing_padding_rows),
    );
    if let Some(bottom) = surface_bottom_row(plan, theme) {
        rows.push(fit_line(&format!("{frame_left}{bottom}"), outer_width));
    }
    rows
}
