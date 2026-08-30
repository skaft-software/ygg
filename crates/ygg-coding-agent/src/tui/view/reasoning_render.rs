use sexy_tui_rs::{strip_terminal_sequences, Color, RichRenderer};

use super::assistant_block::AssistantBlock;
use super::{activity_elbow, finish_transcript_block, fit_line, subdued_text};
use crate::tui::terminal::ColorDepth;
use crate::tui::theme::YggTheme;

const ACTIVITY_SHIMMER_FRAMES: usize = 12;
/// The status label starts two cells after its margin dot (`• `). Keeping the
/// dot in the same coordinate space makes the shimmer travel through it before
/// crossing the label.
const ACTIVITY_LABEL_OFFSET: isize = 2;
const ACTIVITY_MARKER_INDEX: isize = -ACTIVITY_LABEL_OFFSET;
type Rgb = (u8, u8, u8);
const ACTIVITY_RAINBOW: [Rgb; 7] = [
    (255, 96, 96),
    (255, 176, 72),
    (240, 224, 88),
    (104, 220, 128),
    (80, 200, 232),
    (120, 152, 255),
    (216, 120, 240),
];

fn mix_channel(base: u8, accent: u8, strength_percent: u16) -> u8 {
    let base = u32::from(base);
    let accent = u32::from(accent);
    let strength = u32::from(strength_percent.min(100));
    ((base * (100 - strength) + accent * strength + 50) / 100) as u8
}

/// Return the normal model colour and the background-adjacent shadow colour.
/// The shadow is used as a foreground only; it must never become a background
/// fill around the status text.
fn activity_shimmer_palette(theme: &YggTheme, reasoning: &AssistantBlock) -> Option<(Rgb, Rgb)> {
    let capabilities = theme.capabilities();
    if !capabilities.animation
        || !capabilities.interactive
        || capabilities.color == ColorDepth::None
    {
        return None;
    }
    let model = theme.model_rgb(reasoning.model_lab)?;
    let shadow = theme.composer_idle_rgb(model);
    Some((model, shadow))
}

fn rainbow_index(index: isize, shimmer_frame: usize) -> usize {
    (index - (shimmer_frame % ACTIVITY_RAINBOW.len()) as isize)
        .rem_euclid(ACTIVITY_RAINBOW.len() as isize) as usize
}

fn activity_shimmer_color(
    model: (u8, u8, u8),
    shadow: (u8, u8, u8),
    label: &str,
    index: isize,
    shimmer_frame: usize,
    rainbow_strength: u16,
) -> (u8, u8, u8) {
    let center = (shimmer_frame % ACTIVITY_SHIMMER_FRAMES) as isize - ACTIVITY_LABEL_OFFSET;
    let shadow_strength = match (index - center).unsigned_abs() {
        0 => 100,
        1 => 78,
        2 => 48,
        _ => 28,
    };
    // Keep the muted colour as the baseline and let the model colour form the
    // moving highlight. This is the original lower-contrast treatment.
    let normal = (
        mix_channel(shadow.0, model.0, shadow_strength),
        mix_channel(shadow.1, model.1, shadow_strength),
        mix_channel(shadow.2, model.2, shadow_strength),
    );
    if label == "Working" && rainbow_strength > 0 {
        let rainbow = ACTIVITY_RAINBOW[rainbow_index(index, shimmer_frame)];
        (
            mix_channel(normal.0, rainbow.0, rainbow_strength),
            mix_channel(normal.1, rainbow.1, rainbow_strength),
            mix_channel(normal.2, rainbow.2, rainbow_strength),
        )
    } else {
        normal
    }
}

fn activity_shimmer_label(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    label: &str,
    shimmer_frame: usize,
    rainbow_strength: u16,
) -> String {
    let static_label = || theme.bold(&theme.model_fg(reasoning.model_lab, label));
    let Some((model, shadow)) = activity_shimmer_palette(theme, reasoning) else {
        return static_label();
    };
    let mut rendered = String::with_capacity(label.len().saturating_mul(20));
    for (index, character) in label.chars().enumerate() {
        let color = activity_shimmer_color(
            model,
            shadow,
            label,
            index as isize,
            shimmer_frame,
            rainbow_strength,
        );
        let mut encoded = [0; 4];
        let character = character.encode_utf8(&mut encoded);
        rendered.push_str(&theme.rgb_fg(color, character));
    }
    theme.bold(&rendered)
}

fn activity_label(reasoning: &AssistantBlock) -> &str {
    if reasoning.is_working_activity() {
        "Working"
    } else if reasoning.text.is_empty() && !reasoning.show_reasoning_hint {
        reasoning.reasoning_heading.as_deref().unwrap_or("Thinking")
    } else {
        "Thinking"
    }
}

/// Render the margin dot in the same shimmer coordinate space as the status
/// label. Every style is a foreground style applied only to the dot glyph.
pub(super) fn activity_shimmer_marker(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    shimmer_frame: usize,
    rainbow_strength: u16,
    marker: &str,
) -> String {
    let static_marker = || theme.model_fg(reasoning.model_lab, marker);
    let Some((model, shadow)) = activity_shimmer_palette(theme, reasoning) else {
        return static_marker();
    };
    let color = activity_shimmer_color(
        model,
        shadow,
        activity_label(reasoning),
        ACTIVITY_MARKER_INDEX,
        shimmer_frame,
        rainbow_strength,
    );
    theme.rgb_fg(color, marker)
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

fn format_activity_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!(
            "{}h{:02}m{:02}s",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        )
    }
}

fn activity_status_line(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    label: &str,
    shimmer_frame: usize,
    rainbow_strength: u16,
) -> String {
    let label = activity_shimmer_label(theme, reasoning, label, shimmer_frame, rainbow_strength);
    let Some(started_at) = reasoning.activity_started_at else {
        return label;
    };
    let elapsed = format_activity_duration(started_at.elapsed().as_secs());
    let detail = subdued_text(theme, &format!("({elapsed} • esc to interrupt)"));
    format!("{label} {detail}")
}

fn collapsed_reasoning_lines_at(
    theme: &YggTheme,
    reasoning: &AssistantBlock,
    shimmer_frame: usize,
    rainbow_strength: u16,
) -> Vec<String> {
    if reasoning.finished {
        return Vec::new();
    }
    if reasoning.is_working_activity() {
        return vec![activity_status_line(
            theme,
            reasoning,
            "Working",
            shimmer_frame,
            rainbow_strength,
        )];
    }
    if reasoning.text.is_empty() && !reasoning.show_reasoning_hint {
        let label = reasoning.reasoning_heading.as_deref().unwrap_or("Thinking");
        return vec![activity_status_line(
            theme,
            reasoning,
            label,
            shimmer_frame,
            0,
        )];
    }

    let mut lines = vec![activity_status_line(
        theme,
        reasoning,
        "Thinking",
        shimmer_frame,
        0,
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
    collapsed_reasoning_lines_at(theme, reasoning, 0, 0)
}

#[cfg(test)]
pub(super) fn render_reasoning_on_surface(
    reasoning: &AssistantBlock,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    show_reasoning: bool,
    background: Option<Color>,
    shimmer_frame: usize,
) -> Vec<String> {
    render_reasoning_on_surface_with_rainbow(
        reasoning,
        renderer,
        theme,
        width,
        show_reasoning,
        background,
        shimmer_frame,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_reasoning_on_surface_with_rainbow(
    reasoning: &AssistantBlock,
    renderer: &RichRenderer,
    theme: &YggTheme,
    width: u16,
    show_reasoning: bool,
    background: Option<Color>,
    shimmer_frame: usize,
    rainbow_strength: u16,
) -> Vec<String> {
    let non_expandable_activity = reasoning.text.is_empty() && !reasoning.show_reasoning_hint;
    if non_expandable_activity || (!reasoning.reasoning_expanded && !show_reasoning) {
        return collapsed_reasoning_lines_at(theme, reasoning, shimmer_frame, rainbow_strength)
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
    use std::time::{Duration, Instant};

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

    fn foreground_color_codes(rendered: &str) -> Vec<String> {
        rendered
            .split("\x1b[38;2;")
            .skip(1)
            .filter_map(|part| part.split_once('m').map(|(color, _)| color.to_owned()))
            .collect()
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
        let first = collapsed_reasoning_lines_at(&theme, &reasoning, 2, 0);
        let next = collapsed_reasoning_lines_at(&theme, &reasoning, 3, 0);

        assert_eq!(strip_terminal_sequences(&first[0]), "Thinking");
        assert_eq!(
            strip_terminal_sequences(&first[1]),
            "└ Verifying implementation (ctrl+o to expand)"
        );
        assert!(first[0].contains("\x1b[1m"), "{first:?}");
        assert!(!first[1].contains("\x1b[3m"), "{first:?}");
        assert_ne!(first[0], next[0], "the Thinking shimmer must move");
        assert!(first[0].contains("38;2;"), "{first:?}");
        assert!(
            !first[0].contains(";48;2;"),
            "the Codex-style shimmer must never paint character backgrounds: {first:?}"
        );
    }

    #[test]
    fn model_and_rainbow_shimmers_are_foreground_only() {
        let theme = theme::test_theme();
        let reasoning =
            AssistantBlock::streaming_reasoning("").with_model_lab(Some(ModelLab::Alibaba));
        let mut working = reasoning;
        working.reasoning_heading = Some("Working".into());
        working.show_reasoning_hint = false;

        let normal = collapsed_reasoning_lines_at(&theme, &working, 0, 0);
        assert!(normal[0].contains("38;2;"), "{normal:?}");
        assert!(!normal[0].contains(";48;2;"), "{normal:?}");

        let rainbow = collapsed_reasoning_lines_at(&theme, &working, 0, 100);
        assert!(rainbow[0].contains("38;2;"), "{rainbow:?}");
        assert!(!rainbow[0].contains(";48;2;"), "{rainbow:?}");
        assert_ne!(normal[0], rainbow[0]);

        let plain =
            theme::test_theme_with(TerminalCapabilities::test(false, false, ColorDepth::None));
        let no_color = collapsed_reasoning_lines_at(&plain, &working, 0, 100);
        assert_eq!(no_color, vec!["Working"]);
        assert!(!no_color[0].contains('\x1b'));
    }

    #[test]
    fn model_shimmer_keeps_a_muted_baseline_behind_a_moving_highlight() {
        let theme = theme::test_theme();
        let reasoning =
            AssistantBlock::streaming_reasoning("").with_model_lab(Some(ModelLab::Alibaba));
        let model = theme
            .model_rgb(Some(ModelLab::Alibaba))
            .expect("model colour");
        let shadow = theme.composer_idle_rgb(model);
        let rendered = activity_shimmer_label(&theme, &reasoning, "Thinking", 2, 0);
        let colors = foreground_color_codes(&rendered);
        assert_eq!(colors.len(), "Thinking".chars().count(), "{rendered:?}");
        assert_eq!(colors[0], format!("{};{};{}", model.0, model.1, model.2));
        assert_ne!(colors[0], format!("{};{};{}", shadow.0, shadow.1, shadow.2));
        assert_ne!(&colors[0], colors.last().expect("last shimmer colour"));
        assert!(!rendered.contains("\x1b[48;"), "{rendered:?}");
    }

    #[test]
    fn max_rainbow_shimmer_moves_right_at_one_status_frame_per_step() {
        let theme = theme::test_theme();
        let reasoning =
            AssistantBlock::streaming_reasoning("").with_model_lab(Some(ModelLab::Alibaba));
        let frame_zero = foreground_color_codes(&activity_shimmer_label(
            &theme, &reasoning, "Working", 0, 100,
        ));
        let frame_one = foreground_color_codes(&activity_shimmer_label(
            &theme, &reasoning, "Working", 1, 100,
        ));
        let frame_two = foreground_color_codes(&activity_shimmer_label(
            &theme, &reasoning, "Working", 2, 100,
        ));

        assert_eq!(frame_zero.len(), "Working".chars().count());
        assert_eq!(&frame_one[1..], &frame_zero[..6]);
        assert_eq!(&frame_two[2..], &frame_zero[..5]);
    }

    #[test]
    fn activity_marker_shares_the_status_shimmer_phase() {
        let theme = theme::test_theme();
        let reasoning =
            AssistantBlock::streaming_reasoning("private").with_model_lab(Some(ModelLab::Alibaba));
        let first = activity_shimmer_marker(&theme, &reasoning, 0, 0, "•");
        let next = activity_shimmer_marker(&theme, &reasoning, 1, 0, "•");

        assert_ne!(first, next);
        assert!(first.contains("\x1b[38;2;"), "{first:?}");
        assert!(!first.contains("\x1b[48;"), "{first:?}");
        let model = theme
            .model_rgb(Some(ModelLab::Alibaba))
            .expect("model colour");
        let shadow = theme.composer_idle_rgb(model);
        let expected = theme.rgb_fg(
            activity_shimmer_color(model, shadow, "Thinking", ACTIVITY_MARKER_INDEX, 0, 0),
            "•",
        );
        assert_eq!(first, expected);
    }

    #[test]
    fn activity_durations_use_compact_clock_units() {
        assert_eq!(format_activity_duration(28), "28s");
        assert_eq!(format_activity_duration(376), "6m16s");
        assert_eq!(format_activity_duration(3661), "1h01m01s");
    }

    #[test]
    fn working_status_reports_root_run_elapsed_time_and_interrupt_hint() {
        let theme =
            theme::test_theme_with(TerminalCapabilities::test(false, true, ColorDepth::None));
        let mut working = AssistantBlock::streaming_reasoning("")
            .with_model_lab(Some(ModelLab::OpenAi))
            .with_activity_started_at(Some(Instant::now() - Duration::from_millis(28_100)));
        working.reasoning_heading = Some("Working".into());
        working.show_reasoning_hint = false;

        let rendered = collapsed_reasoning_lines_at(&theme, &working, 0, 0);
        assert_eq!(rendered, vec!["Working (28s • esc to interrupt)"]);
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
