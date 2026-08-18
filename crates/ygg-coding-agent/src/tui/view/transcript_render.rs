//! Semantic transcript-block dispatch into focused presenters and surface framing.

use sexy_tui_rs::{visible_width, Color, RichRenderer};

use super::assistant_block::AssistantBlock;
use super::bash_render::{render_bash_row, render_compact_bash_output};
use super::outcome_render::render_outcome;
use super::reasoning_render::render_reasoning_on_surface;
use super::surface_frame::decorate_surface;
use super::surface_layout::{compile_surface_plan, surface_roles};
use super::terminal_text::sanitize_for_terminal;
use super::tool_render::{
    render_compact_tool_output, render_diff_only, tool_diff, tool_display_label, tool_value_indent,
    tool_value_indent_width, without_redundant_tool_lead,
};
use super::transcript_cache::{RenderedTranscriptBlock, SurfaceGeometry};
use super::{
    finish_transcript_block, fit_line, render_shell_output, render_user_prompt, wrap_hanging,
    TranscriptBlock,
};
use crate::tui::theme::{ThemeSurfaceChrome, YggTheme};

#[allow(clippy::too_many_arguments)]
pub(super) fn render_block_planned(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &YggTheme,
    rich_renderer: &RichRenderer,
    reasoning_renderer: &RichRenderer,
    outer_width: u16,
    verbose_tools: bool,
    active_dot_visible: bool,
) -> RenderedTranscriptBlock {
    let layout = theme.layout_for_width(outer_width);
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
            prompt_color.as_deref(),
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
        TranscriptBlock::Tool(panel) => {
            let compact_bash = matches!(panel.name.as_str(), "bash" | "exec")
                && panel.display.shell_command.is_some();
            let tool = if panel.display.shell_command.is_some() {
                "Bash"
            } else {
                tool_display_label(&panel.name)
            };
            let output_indent = tool_value_indent(tool);
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
                // The margin dot owns lifecycle colour. Tool text stays
                // neutral so failures do not wash the whole event red.
                let label = theme.bold(&theme.fg("foreground", tool));
                let text =
                    without_redundant_tool_lead(&panel.name, &sanitize_for_terminal(summary));
                let text = theme.fg("muted", &text);
                let gap = tool_value_indent_width(tool).saturating_sub(visible_width(tool));
                let label_prefix = format!("{label}{}", " ".repeat(gap));
                let continuation = " ".repeat(visible_width(&label_prefix));
                wrap_hanging(&text, &label_prefix, &continuation, width)
            };

            if !panel.is_error {
                match panel.name.as_str() {
                    "bash" | "exec" if compact_bash => lines.extend(render_compact_bash_output(
                        panel,
                        theme,
                        width,
                        verbose_tools,
                        layout.show_tool_duration,
                        &output_indent,
                    )),
                    "search" => lines.extend(render_compact_tool_output(
                        panel,
                        theme,
                        width,
                        verbose_tools,
                        &output_indent,
                    )),
                    "edit" | "write" if tool_diff(panel).is_some() => {
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
        TranscriptBlock::User { prompt_color, .. } => prompt_color.as_deref(),
        _ => None,
    };
    let lines = decorate_surface(
        lines,
        block,
        &plan,
        theme,
        outer_width,
        prompt_color,
        active_dot_visible,
        collapsed_reasoning,
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
        true,
    )
    .lines
}
