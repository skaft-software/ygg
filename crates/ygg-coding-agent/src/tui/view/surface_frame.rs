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

pub(super) fn event_margin_marker(
    block: &TranscriptBlock,
    theme: &YggTheme,
    active_dot_visible: bool,
    collapsed_reasoning: bool,
) -> Option<String> {
    let active_dot = if theme.unicode() { "•" } else { "*" };
    let quiet_dot = if theme.unicode() { "·" } else { "." };
    let active_phase_dot = if active_dot_visible {
        active_dot
    } else {
        quiet_dot
    };
    match block {
        TranscriptBlock::User { .. } | TranscriptBlock::Outcome(_) | TranscriptBlock::Notice(_) => {
            None
        }
        TranscriptBlock::Reasoning(reasoning) if collapsed_reasoning => {
            Some(if active_dot_visible {
                theme.model_fg(reasoning.model_lab, active_dot)
            } else {
                " ".to_owned()
            })
        }
        TranscriptBlock::Reasoning(_) => None,
        TranscriptBlock::Tool(panel) if !panel.finished => {
            Some(theme.fg("foreground", active_phase_dot))
        }
        TranscriptBlock::Tool(panel) if panel.is_error => {
            Some(theme.settled_event_dot("error", quiet_dot))
        }
        TranscriptBlock::Tool(panel) if matches!(panel.name.as_str(), "bash" | "exec") => {
            Some(theme.settled_event_dot("success", quiet_dot))
        }
        TranscriptBlock::Shell(shell) if shell.running => {
            Some(theme.fg("foreground", active_phase_dot))
        }
        TranscriptBlock::Shell(shell) => Some(theme.settled_event_dot(
            if shell.exit_code == 0 {
                "success"
            } else {
                "error"
            },
            quiet_dot,
        )),
        _ => Some(theme.settled_event_dot("neutral", quiet_dot)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decorate_surface(
    content: Vec<String>,
    block: &TranscriptBlock,
    plan: &SurfacePlan<'_>,
    theme: &YggTheme,
    outer_width: u16,
    prompt_color: Option<&str>,
    active_dot_visible: bool,
    collapsed_reasoning: bool,
) -> Vec<String> {
    let mut rows = Vec::with_capacity(
        plan.geometry.transition_rows
            + plan.geometry.leading_rows
            + content.len()
            + plan.geometry.trailing_rows,
    );
    rows.extend(std::iter::repeat_n(
        String::new(),
        plan.geometry.transition_rows,
    ));
    if plan.geometry.leading_rows > 0 {
        rows.push(styled_surface_heading(plan, theme));
    }
    if plan.geometry.leading_rows > 1 {
        rows.extend(std::iter::repeat_n(
            render_surface_content_line("", plan, theme, prompt_color),
            plan.geometry.leading_rows - 1,
        ));
    }
    rows.extend(
        content
            .iter()
            .map(|line| render_surface_content_line(line, plan, theme, prompt_color)),
    );
    if plan.geometry.trailing_rows > 1 {
        rows.extend(std::iter::repeat_n(
            render_surface_content_line("", plan, theme, prompt_color),
            plan.geometry.trailing_rows - 1,
        ));
    }
    if plan.geometry.trailing_rows > 0 {
        let (_, border_role, _) = surface_roles(plan.kind);
        let middle = horizontal_rule(theme, usize::from(plan.frame_width.saturating_sub(2)));
        let bottom = format!(
            "{}{}{}",
            theme.glyph("bottom_left"),
            middle,
            theme.glyph("bottom_right")
        );
        rows.push(theme.apply_semantic_role_layered(border_role, &bottom));
    }

    let marker = event_margin_marker(block, theme, active_dot_visible, collapsed_reasoning);
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
