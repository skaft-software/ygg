//! Semantic transcript-block dispatch into focused presenters and surface framing.

use sexy_tui_rs::{visible_width, Color, RichRenderer};

use super::assistant_block::AssistantBlock;
use super::bash_render::{render_bash_row, render_compact_bash_output};
use super::outcome_render::render_outcome;
use super::reasoning_render::render_reasoning_on_surface;
use super::surface_frame::{
    decorate_surface_content_suffix, decorate_surface_with_frame, event_margin_marker_with_frame,
};
use super::surface_layout::{compile_surface_plan, surface_roles};
use super::terminal_text::sanitize_for_terminal;
use super::tool_render::{
    render_compact_tool_output, render_diff_only, tool_diff, tool_display_label, tool_value_indent,
    tool_value_indent_width, without_redundant_tool_lead,
};
use super::transcript_cache::{RenderedTranscriptBlock, SurfaceGeometry};
use super::{
    finish_transcript_block, fit_line, render_shell_output, render_user_prompt, wrap_hanging,
    ToolPanel, TranscriptBlock,
};
use crate::tui::theme::{ThemeSurfaceChrome, YggTheme};

fn extension_activity_state_label(state: ygg_agent::ExtensionPresentationState) -> &'static str {
    match state {
        ygg_agent::ExtensionPresentationState::Loading => "loading",
        ygg_agent::ExtensionPresentationState::Pending => "pending",
        ygg_agent::ExtensionPresentationState::Active => "active",
        ygg_agent::ExtensionPresentationState::Running => "running",
        ygg_agent::ExtensionPresentationState::Succeeded => "completed",
        ygg_agent::ExtensionPresentationState::Failed => "failed",
        ygg_agent::ExtensionPresentationState::Cancelled => "cancelled",
        ygg_agent::ExtensionPresentationState::Degraded => "degraded",
        ygg_agent::ExtensionPresentationState::Stopped => "stopped",
        ygg_agent::ExtensionPresentationState::Unavailable => "unavailable",
        ygg_agent::ExtensionPresentationState::Empty => "empty",
    }
}

fn render_subagent_activity_panel(panel: &ToolPanel, theme: &YggTheme, width: u16) -> Vec<String> {
    let Some(view) = panel.subagent_activity.as_ref() else {
        return Vec::new();
    };
    let label = theme.bold(&theme.fg("foreground", "Subagents"));
    let mut lines = vec![label];
    // A subagents event is already a bounded roster (at most eight workers),
    // so keep every child visible even when ordinary tool output is collapsed.
    let unicode = theme.unicode();

    if !view.telemetry.is_empty() {
        let children = &view.telemetry;
        let task_width = children
            .iter()
            .map(|child| visible_width(&sanitize_for_terminal(&child.task_name)))
            .max()
            .unwrap_or_default();
        for (index, child) in children.iter().rev().enumerate() {
            let last = index + 1 == children.len();
            let elbow = match (unicode, last) {
                (true, true) => "└",
                (true, false) => "├",
                (false, true) => "`-",
                (false, false) => "+-",
            };
            let task = sanitize_for_terminal(&child.task_name);
            let task = format!("{task:<task_width$}");
            let status = if child.state.is_empty() {
                "running"
            } else {
                child.state.as_str()
            };
            let mut detail = status.to_owned();
            if matches!(child.state.as_str(), "pending" | "running") {
                if let Some(tool) = child.current_tool.as_deref() {
                    detail.push_str(" · ");
                    detail.push_str(&sanitize_for_terminal(tool));
                }
            }
            let calls = child.tool_use_count;
            detail.push_str(" · ");
            detail.push_str(&format!(
                "{calls} call{}",
                if calls == 1 { "" } else { "s" }
            ));
            // Live token and cost telemetry, matching the composer chrome
            // strip. Input buckets all occupy context, so cache reads and
            // writes are folded into the prompt-side count.
            let input = child
                .input_tokens
                .saturating_add(child.cache_read_tokens)
                .saturating_add(child.cache_write_tokens);
            if unicode {
                detail.push_str(&format!(
                    " · ↑{} ↓{}",
                    crate::tui::composer_surface::compact_token_count(input),
                    crate::tui::composer_surface::compact_token_count(child.output_tokens),
                ));
            } else {
                detail.push_str(&format!(
                    " · in {} out {}",
                    crate::tui::composer_surface::compact_token_count(input),
                    crate::tui::composer_surface::compact_token_count(child.output_tokens),
                ));
            }
            if let Some(cost) = child.cost_microdollars {
                detail.push_str(if unicode { " • " } else { " - " });
                detail.push_str(&crate::tui::composer_surface::format_microdollars(cost));
            }
            lines.push(fit_line(
                &format!(
                    "  {} {} {}",
                    theme.fg("muted", elbow),
                    theme.fg("foreground", &task),
                    theme.fg("muted", &detail),
                ),
                width,
            ));
        }
    } else {
        let activities = &view.activities;
        let summary_width = activities
            .iter()
            .map(|activity| visible_width(&sanitize_for_terminal(&activity.summary)))
            .max()
            .unwrap_or_default();
        for (index, activity) in activities.iter().rev().enumerate() {
            let last = index + 1 == activities.len();
            let elbow = match (unicode, last) {
                (true, true) => "└",
                (true, false) => "├",
                (false, true) => "`-",
                (false, false) => "+-",
            };
            let summary = sanitize_for_terminal(&activity.summary);
            let summary = format!("{summary:<summary_width$}");
            let calls = activity.metrics.map_or(0, |metrics| metrics.tool_calls);
            let detail = format!(
                "{} · {calls} call{}",
                extension_activity_state_label(activity.state),
                if calls == 1 { "" } else { "s" }
            );
            lines.push(fit_line(
                &format!(
                    "  {} {} {}",
                    theme.fg("muted", elbow),
                    theme.fg("foreground", &summary),
                    theme.fg("muted", &detail),
                ),
                width,
            ));
        }
    }

    if lines.len() == 1 {
        if let Some(reason) = view.failure_reason.as_deref() {
            lines.push(fit_line(
                &format!(
                    "  {} {}",
                    theme.fg("muted", if unicode { "└" } else { "`-" }),
                    theme.fg(
                        "muted",
                        &format!("failed · {}", sanitize_for_terminal(reason))
                    ),
                ),
                width,
            ));
        }
    }
    finish_transcript_block(lines)
}

pub(super) struct RenderedTranscriptBlockUpdate {
    pub(super) stable_rows: usize,
    pub(super) replacement: Vec<String>,
    pub(super) geometry: SurfaceGeometry,
}

/// Incrementally decorate a streaming assistant tail. Stable Markdown rows and
/// their outer surface frame remain in `TranscriptCache`; only the mutable
/// content suffix plus trailing frame rows are rebuilt.
pub(super) fn render_assistant_update_planned(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &YggTheme,
    rich_renderer: &RichRenderer,
    outer_width: u16,
) -> Option<RenderedTranscriptBlockUpdate> {
    let TranscriptBlock::Assistant(assistant) = block else {
        return None;
    };
    let plan = compile_surface_plan(previous, block, theme, outer_width);
    let update = assistant.render_update(rich_renderer, theme, plan.geometry.content_width)?;
    if update.stable_prefix == 0 {
        return None;
    }

    let stable_rows = plan
        .geometry
        .transition_rows
        .saturating_add(plan.geometry.leading_rows)
        .saturating_add(update.stable_prefix);
    let replacement =
        decorate_surface_content_suffix(update.replacement, &plan, theme, outer_width, None, false);
    Some(RenderedTranscriptBlockUpdate {
        stable_rows,
        replacement,
        geometry: plan.geometry,
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_block_planned(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &YggTheme,
    rich_renderer: &RichRenderer,
    reasoning_renderer: &RichRenderer,
    outer_width: u16,
    verbose_tools: bool,
    spinner_frame: usize,
) -> RenderedTranscriptBlock {
    let plan = compile_surface_plan(previous, block, theme, outer_width);
    let width = plan.geometry.content_width;
    let content_background = matches!(
        plan.chrome,
        ThemeSurfaceChrome::Card | ThemeSurfaceChrome::Band
    )
    .then(|| theme.semantic_style(surface_roles(plan.kind).0).background)
    .filter(|background| *background != Color::Default);
    let collapsed_reasoning = matches!(
        block,
        TranscriptBlock::Reasoning(reasoning)
            if !reasoning.reasoning_expanded && !verbose_tools
    );
    let lines = match block {
        TranscriptBlock::User {
            text,
            model_lab,
            prompt_color,
            ..
        } => render_user_prompt(
            text,
            model_lab,
            prompt_color
                .as_deref()
                .filter(|_| theme.uses_model_lab_color()),
            rich_renderer,
            theme,
            width,
        ),
        TranscriptBlock::Assistant(assistant) => finish_transcript_block(
            assistant.render_on_surface(rich_renderer, theme, width, content_background),
        ),
        TranscriptBlock::Reasoning(reasoning) => render_reasoning_on_surface(
            reasoning,
            reasoning_renderer,
            theme,
            width,
            verbose_tools,
            content_background,
        ),
        TranscriptBlock::Tool(panel) if panel.subagent_activity.is_some() => {
            finish_transcript_block(render_subagent_activity_panel(panel, theme, width))
        }
        TranscriptBlock::Tool(panel) => {
            let compact_bash = matches!(panel.name.as_str(), "bash" | "exec")
                && panel.display.shell_command.is_some();
            let tool = if panel.display.shell_command.is_some() {
                "Bash".to_string()
            } else {
                tool_display_label(&panel.name)
            };
            let output_indent = tool_value_indent(&tool);
            let mut lines = if let Some(command) = panel.display.shell_command.as_deref() {
                render_bash_row(command, rich_renderer, theme, width)
            } else {
                let compact = width < 60;
                let summary = if !panel.finished {
                    if compact {
                        &panel.display.compact_active
                    } else {
                        &panel.display.active
                    }
                } else if panel.is_error {
                    if compact {
                        &panel.display.compact_failure
                    } else {
                        &panel.display.failure
                    }
                } else if compact {
                    &panel.display.compact_success
                } else {
                    &panel.display.success
                };
                let tool = tool_display_label(&panel.name);
                let label = theme.bold(&theme.fg("foreground", &tool));
                let label_width = visible_width(&tool);
                let text = match panel.display.value.as_deref() {
                    Some(value) => sanitize_for_terminal(value),
                    None => {
                        without_redundant_tool_lead(&panel.name, &sanitize_for_terminal(summary))
                    }
                };
                let text = theme.fg("muted", &text);
                let gap = tool_value_indent_width(&tool).saturating_sub(label_width);
                let label_prefix = format!("{label}{}", " ".repeat(gap));
                let continuation = " ".repeat(visible_width(&label_prefix));
                wrap_hanging(&text, &label_prefix, &continuation, width)
            };

            match panel.name.as_str() {
                "bash" | "exec" if compact_bash => lines.extend(render_compact_bash_output(
                    panel,
                    theme,
                    width,
                    verbose_tools,
                    &output_indent,
                )),
                "search" if !panel.is_error => lines.extend(render_compact_tool_output(
                    panel,
                    theme,
                    width,
                    verbose_tools,
                    &output_indent,
                )),
                "edit" | "write" if !panel.is_error && tool_diff(panel).is_some() => {
                    lines.extend(render_diff_only(
                        panel,
                        rich_renderer,
                        theme,
                        width,
                        verbose_tools,
                        &output_indent,
                    ))
                }
                _ => {}
            }
            finish_transcript_block(lines)
        }
        TranscriptBlock::Outcome(outcome) => render_outcome(outcome, theme, width),
        TranscriptBlock::Notice(text) => {
            let text = theme.fg("muted", &sanitize_for_terminal(text));
            finish_transcript_block(wrap_hanging(&text, "", "", width))
        }
        TranscriptBlock::NoticeStatus { text, .. } => {
            let text = theme.fg("muted", &sanitize_for_terminal(text));
            finish_transcript_block(wrap_hanging(&text, "", "", width))
        }
        TranscriptBlock::Compaction(compaction) => {
            let marker = theme.glyph("note");
            let prefix = format!("{} ", theme.fg("model_accent", marker));
            let continuation = " ".repeat(visible_width(&prefix));
            let expanded = compaction.expanded || verbose_tools;
            let action = if expanded {
                "ctrl+o to collapse"
            } else {
                "ctrl+o to view"
            };
            let label = format!("{} · ({action})", sanitize_for_terminal(&compaction.label));
            let mut lines = wrap_hanging(&label, &prefix, &continuation, width);
            if expanded {
                let summary = AssistantBlock::finalized(compaction.summary.clone());
                let summary_width = width.saturating_sub(2).max(1);
                lines.extend(
                    summary
                        .render_on_surface(rich_renderer, theme, summary_width, content_background)
                        .into_iter()
                        .map(|line| {
                            if line.is_empty() {
                                String::new()
                            } else {
                                fit_line(&format!("  {line}"), width)
                            }
                        }),
                );
            }
            finish_transcript_block(lines)
        }
        TranscriptBlock::Shell(shell) => {
            let marker = theme.glyph("shell");
            let prefix = format!("{} ", theme.bold(&theme.fg("model_accent", marker)));
            let status = if shell.running {
                theme.dim("…")
            } else if shell.exit_code == 0 {
                theme.dim("[ok]")
            } else {
                theme.fg("error", "[failed]")
            };
            let mut lines = vec![fit_line(
                &format!(
                    "{} {} {}",
                    prefix,
                    theme.dim(&sanitize_for_terminal(&shell.command)),
                    status,
                ),
                width,
            )];
            lines.extend(render_shell_output(shell, theme, width, verbose_tools));
            finish_transcript_block(lines)
        }
    };

    if lines.is_empty() {
        return RenderedTranscriptBlock {
            lines,
            geometry: SurfaceGeometry::default(),
        };
    }
    let prompt_color = match block {
        TranscriptBlock::User { prompt_color, .. } => prompt_color
            .as_deref()
            .filter(|_| theme.uses_model_lab_color()),
        _ => None,
    };
    let marker = event_margin_marker_with_frame(block, theme, spinner_frame, collapsed_reasoning);
    let lines = decorate_surface_with_frame(
        lines,
        &plan,
        theme,
        outer_width,
        prompt_color,
        collapsed_reasoning,
        marker,
    );
    RenderedTranscriptBlock {
        lines,
        geometry: plan.geometry,
    }
}

#[cfg(test)]
pub(super) fn render_block(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &YggTheme,
    rich_renderer: &RichRenderer,
    reasoning_renderer: &RichRenderer,
    outer_width: u16,
    verbose_tools: bool,
) -> Vec<String> {
    render_block_planned(
        previous,
        block,
        theme,
        rich_renderer,
        reasoning_renderer,
        outer_width,
        verbose_tools,
        0,
    )
    .lines
}
