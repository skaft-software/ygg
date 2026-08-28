use sexy_tui_rs::{strip_terminal_sequences, Color, RichRenderer};

use super::assistant_block::AssistantBlock;
use super::{activity_elbow, finish_transcript_block, fit_line, subdued_text};
use crate::tui::terminal::ColorDepth;
use crate::tui::theme::YggTheme;

fn mix_channel(base: u8, accent: u8, strength_percent: u16) -> u8 {
    let base = u32::from(base);
    let accent = u32::from(accent);
    let strength = u32::from(strength_percent.min(100));
    ((base * (100 - strength) + accent * strength + 50) / 100) as u8
}

fn activity_shimmer_label(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    label: &str,
    shimmer_frame: usize,
) -> String {
    const SHIMMER_FRAMES: usize = 12;
    let static_label = || theme.bold(&theme.model_fg(reasoning.model_lab, label));
    let capabilities = theme.capabilities();
    if !capabilities.animation
        || !capabilities.interactive
        || capabilities.color == ColorDepth::None
    {
        return static_label();
    }
    let Some(accent) = theme.model_rgb(reasoning.model_lab) else {
        return static_label();
    };
    let base = theme.composer_idle_rgb(accent);
    let center = (shimmer_frame % SHIMMER_FRAMES) as isize - 2;
    let mut rendered = String::with_capacity(label.len().saturating_mul(20));
    for (index, character) in label.chars().enumerate() {
        let strength = match (index as isize - center).unsigned_abs() {
            0 => 100,
            1 => 78,
            2 => 48,
            _ => 28,
        };
        let color = (
            mix_channel(base.0, accent.0, strength),
            mix_channel(base.1, accent.1, strength),
            mix_channel(base.2, accent.2, strength),
        );
        let mut encoded = [0; 4];
        rendered.push_str(&theme.rgb_fg(color, character.encode_utf8(&mut encoded)));
    }
    theme.bold(&rendered)
}

fn reasoning_detail_line(theme: &YggTheme, reasoning: &AssistantBlock) -> String {
    let elbow = activity_elbow(theme);
    let hint = "(ctrl+o to expand)";
    let detail = reasoning.reasoning_heading.as_deref().map_or_else(
        || format!("{elbow} {hint}"),
        |heading| format!("{elbow} {heading} {hint}"),
    );
    subdued_text(theme, &detail)
}

fn collapsed_reasoning_lines_at(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    shimmer_frame: usize,
) -> Vec<String> {
    if reasoning.finished {
        return Vec::new();
    }
    if reasoning.is_working_activity() {
        return vec![activity_shimmer_label(
            theme,
            reasoning,
            "Working",
            shimmer_frame,
        )];
    }
    if reasoning.text.is_empty() && !reasoning.show_reasoning_hint {
        let label = reasoning.reasoning_heading.as_deref().unwrap_or("Thinking");
        return vec![theme.model_fg(reasoning.model_lab, label)];
    }

    let mut lines = vec![activity_shimmer_label(
        theme,
        reasoning,
        "Thinking",
        shimmer_frame,
    )];
    if reasoning.show_reasoning_hint {
        lines.push(reasoning_detail_line(theme, reasoning));
    }
    lines
}

pub(super) fn collapsed_reasoning_lines(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
) -> Vec<String> {
    collapsed_reasoning_lines_at(theme, reasoning, 0)
}

pub(super) fn render_reasoning_on_surface(
    reasoning: &AssistantBlock,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    show_reasoning: bool,
    background: Option<Color>,
    shimmer_frame: usize,
) -> Vec<String> {
    let non_expandable_activity = reasoning.text.is_empty() && !reasoning.show_reasoning_hint;
    if non_expandable_activity || (!reasoning.reasoning_expanded && !show_reasoning) {
        return collapsed_reasoning_lines_at(theme, reasoning, shimmer_frame)
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

    // Expanded reasoning already owns a distinct transcript inset and muted
    // prose style. Do not turn its first row into a one-item bulleted list;
    // every Markdown row starts from the same reasoning content gutter.
    finish_transcript_block(reasoning.render_on_surface(renderer, theme, width, background))
        .into_iter()
        .map(|line| fit_line(&line, width))
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
        render_reasoning_on_surface(reasoning, renderer, theme, width, show_reasoning, None, 0)
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
    fn expanded_reasoning_has_no_first_line_bullet() {
        let theme = theme::test_theme();
        let mut reasoning = AssistantBlock::streaming_reasoning(
            "First private thought.\n\nSecond private thought.",
        );
        reasoning.reasoning_expanded = true;

        let lines = render_reasoning(&reasoning, &theme.reasoning_renderer(), &theme, 80, true)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();

        assert!(lines[0].starts_with("First private thought."), "{lines:?}");
        assert!(lines[1].starts_with("Second private thought."), "{lines:?}");
        assert!(
            lines
                .iter()
                .all(|line| !line.starts_with('•') && !line.starts_with('·')),
            "expanded reasoning must not look like a bulleted list: {lines:?}"
        );
    }

    #[test]
    fn collapsed_reasoning_uses_shimmering_thinking_and_moves_heading_to_detail() {
        let theme = theme::test_theme();
        let reasoning = AssistantBlock::streaming_reasoning("## Verifying `implementation`")
            .with_model_lab(Some(ModelLab::Alibaba));
        let first = collapsed_reasoning_lines_at(&theme, &reasoning, 2);
        let next = collapsed_reasoning_lines_at(&theme, &reasoning, 3);

        assert_eq!(strip_terminal_sequences(&first[0]), "Thinking");
        assert_eq!(
            strip_terminal_sequences(&first[1]),
            "└ Verifying implementation (ctrl+o to expand)"
        );
        assert!(first[0].contains("\x1b[1m"), "{first:?}");
        assert!(!first[1].contains("\x1b[3m"), "{first:?}");
        assert_ne!(first[0], next[0], "the Thinking shimmer must move");

        let accent = theme
            .model_rgb(Some(ModelLab::Alibaba))
            .expect("Alibaba model accent");
        assert!(first[0].contains(&format!(
            "\x1b[38;2;{};{};{}mT",
            accent.0, accent.1, accent.2
        )));
    }

    #[test]
    fn collapsed_reasoning_without_a_heading_keeps_the_hint_on_the_detail_row() {
        let theme = theme::test_theme();
        let renderer = theme.reasoning_renderer();
        let mut reasoning =
            AssistantBlock::streaming_reasoning("private").with_model_lab(Some(ModelLab::Alibaba));
        let live = render_reasoning(&reasoning, &renderer, &theme, 80, false);
        assert_eq!(live.len(), 2, "{live:?}");
        assert_eq!(strip_terminal_sequences(&live[0]), "Thinking");
        assert_eq!(strip_terminal_sequences(&live[1]), "└ (ctrl+o to expand)");
        assert!(live[0].contains("\x1b[1m"), "{live:?}");
        assert!(!live[1].contains("\x1b[3m"), "{live:?}");

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
        assert_eq!(lines[0], "Thinking", "{lines:?}");
        assert!(lines[1].starts_with("`- A heading"), "{lines:?}");
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
        assert_eq!(strip_terminal_sequences(&lines[0]), "Working");
        assert!(lines[0].contains("\x1b[1m"), "{lines:?}");

        let verbose = render_reasoning(&activity, &theme.reasoning_renderer(), &theme, 80, true);
        assert_eq!(verbose.len(), 1, "{verbose:?}");
        assert_eq!(strip_terminal_sequences(&verbose[0]), "Working");
    }
}
