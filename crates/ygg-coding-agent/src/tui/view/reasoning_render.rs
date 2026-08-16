use sexy_tui_rs::{strip_terminal_sequences, visible_width, Color, RichRenderer};

use super::assistant_block::AssistantBlock;
use super::{finish_transcript_block, fit_line, subdued_text};
use crate::tui::terminal::ColorDepth;
use crate::tui::theme::YggTheme;

fn live_reasoning_label(theme: &YggTheme, reasoning: &AssistantBlock) -> String {
    let label = reasoning.reasoning_heading.as_deref().unwrap_or("Thinking");
    theme.model_fg(reasoning.model_lab, label)
}

pub(super) fn collapsed_reasoning_lines(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    include_margin_marker: bool,
) -> Vec<String> {
    // Finished reasoning leaves no trace in the collapsed transcript. Genuine
    // private reasoning keeps a stable disclosure row; truthful non-expandable
    // activity such as reasoning-off waits uses only the living status line.
    if reasoning.finished {
        Vec::new()
    } else {
        let label = live_reasoning_label(theme, reasoning);
        let label = if include_margin_marker {
            format!(
                "{} {label}",
                theme.model_fg(reasoning.model_lab, theme.glyph("bullet"))
            )
        } else {
            label
        };
        let disclosure_indent = if include_margin_marker { "  " } else { "" };
        let mut lines = vec![label];
        if reasoning.show_reasoning_hint {
            let separator = if theme.unicode() { "·" } else { "." };
            lines.push(subdued_text(
                theme,
                &format!(
                    "{disclosure_indent}{} ctrl+o {separator} unfold",
                    theme.glyph("last_branch"),
                ),
            ));
        }
        lines
    }
}

pub(super) fn render_reasoning_on_surface(
    reasoning: &AssistantBlock,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    show_reasoning: bool,
    background: Option<Color>,
    use_margin_marker: bool,
) -> Vec<String> {
    let marker = theme.glyph("reasoning");
    let prefix_width = visible_width(marker).saturating_add(1);
    let non_expandable_activity = reasoning.text.is_empty() && !reasoning.show_reasoning_hint;
    if non_expandable_activity || (!reasoning.reasoning_expanded && !show_reasoning) {
        return collapsed_reasoning_lines(theme, reasoning, !use_margin_marker)
            .into_iter()
            .map(|line| {
                let line = fit_line(&line, width);
                if theme.capabilities().color == ColorDepth::None {
                    strip_terminal_sequences(&line)
                } else {
                    line
                }
            })
            .collect();
    }
    let content_width = width.saturating_sub(prefix_width as u16).max(1);
    let lines = finish_transcript_block(reasoning.render_on_surface(
        renderer,
        theme,
        content_width,
        background,
    ));

    lines
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            if line.is_empty() {
                String::new()
            } else if index == 0 {
                fit_line(&format!("{} {line}", theme.fg("muted", marker)), width)
            } else {
                fit_line(&format!("{}{line}", " ".repeat(prefix_width)), width)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sexy_tui_rs::{strip_terminal_sequences, visible_width};

    use super::*;
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};
    use crate::tui::theme::{self, ModelLab};

    fn render_reasoning(
        reasoning: &AssistantBlock,
        renderer: &RichRenderer,
        theme: &YggTheme,
        width: u16,
        show_reasoning: bool,
    ) -> Vec<String> {
        render_reasoning_on_surface(
            reasoning,
            renderer,
            theme,
            width,
            show_reasoning,
            None,
            false,
        )
    }

    #[test]
    fn verbose_reasoning_deltas_keep_complete_incremental_state() {
        let theme = theme::test_theme();
        let mut reasoning = AssistantBlock::streaming_reasoning("First complete thought.\n\n");
        let initial_revision = reasoning.markdown.tail_revision();

        for step in 0..256 {
            reasoning.append_reasoning(&format!("Thought {step} stays visible.\n\n"));
        }

        assert!(
            reasoning.markdown.tail_revision() >= initial_revision + 256,
            "ordinary deltas must extend one incremental Markdown stream"
        );
        reasoning.reasoning_expanded = true;
        let live = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 80, true)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(live.contains("First complete thought."), "{live}");
        assert!(live.contains("Thought 0 stays visible."), "{live}");
        assert!(live.contains("Thought 255 stays visible."), "{live}");

        reasoning.finish_reasoning();
        let finished = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 80, true)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(finished.contains("First complete thought."), "{finished}");
        assert!(finished.contains("Thought 0 stays visible."), "{finished}");
        assert!(
            finished.contains("Thought 255 stays visible."),
            "{finished}"
        );
    }

    #[test]
    fn collapsed_reasoning_label_is_plain_model_colored_text() {
        let theme = theme::test_theme();
        let reasoning = AssistantBlock::streaming_reasoning("## Verifying `implementation`")
            .with_model_lab(Some(ModelLab::Alibaba));
        let label = live_reasoning_label(&theme, &reasoning);
        assert_eq!(strip_terminal_sequences(&label), "Verifying implementation");
        assert!(!label.contains("\x1b[1m"), "{label:?}");
        let accent = theme
            .model_rgb(Some(ModelLab::Alibaba))
            .expect("Alibaba model accent");
        assert!(
            label.contains(&format!(
                "\x1b[38;2;{};{};{}m",
                accent.0, accent.1, accent.2
            )),
            "reasoning label must retain the block model's accent: {label:?}"
        );
    }

    #[test]
    fn collapsed_reasoning_shows_two_live_rows_and_no_settled_rows() {
        let theme = theme::test_theme();
        let renderer = theme.reasoning_renderer();
        let mut reasoning =
            AssistantBlock::streaming_reasoning("private").with_model_lab(Some(ModelLab::Alibaba));
        let live = render_reasoning(&reasoning, &renderer, &theme, 80, false);
        assert_eq!(live.len(), 2, "{live:?}");
        assert!(strip_terminal_sequences(&live[0]).contains("• Thinking"));
        assert!(strip_terminal_sequences(&live[1]).contains("└ ctrl+o · unfold"));
        assert!(!live[0].contains("\x1b[1m"), "{live:?}");
        let accent = theme
            .model_rgb(Some(ModelLab::Alibaba))
            .expect("Alibaba model accent");
        assert!(
            live[0].contains(&format!(
                "\x1b[38;2;{};{};{}m",
                accent.0, accent.1, accent.2
            )),
            "reasoning label must retain the block model's accent: {live:?}"
        );

        reasoning.reasoning_elapsed = Some(Duration::from_millis(13_700));
        reasoning.finish_reasoning();
        let settled = render_reasoning(&reasoning, &renderer, &theme, 80, false);
        assert!(
            settled.is_empty(),
            "finished reasoning leaves no trace when collapsed"
        );
    }

    #[test]
    fn collapsed_reasoning_has_ascii_fallback_and_width_bounded_rows() {
        let theme =
            theme::test_theme_with(TerminalCapabilities::test(false, false, ColorDepth::None));
        let reasoning = AssistantBlock::streaming_reasoning(
            "## A heading that is intentionally much wider than the viewport\n",
        );
        let lines = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 16, false);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("* A heading"), "{lines:?}");
        assert!(lines[1].starts_with("  `- ctrl+o ."), "{lines:?}");
        assert!(lines.iter().all(|line| visible_width(line) <= 16));
        assert!(lines.iter().all(|line| !line.contains('\x1b')));
    }

    #[test]
    fn non_expandable_activity_is_one_truthful_row() {
        let theme = theme::test_theme();
        let mut activity = AssistantBlock::streaming_reasoning("");
        activity.reasoning_heading = Some("Working".into());
        activity.show_reasoning_hint = false;

        let lines = render_reasoning(&activity, &theme.reasoning_renderer(), &theme, 80, false);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(strip_terminal_sequences(&lines[0]).contains("• Working"));

        let verbose = render_reasoning(&activity, &theme.reasoning_renderer(), &theme, 80, true);
        assert_eq!(verbose.len(), 1, "{verbose:?}");
        assert!(strip_terminal_sequences(&verbose[0]).contains("• Working"));
    }
}
