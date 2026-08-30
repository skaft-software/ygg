//! Terminal presentation for completed, failed, and interrupted runs.

use std::time::Duration;

use crate::presentation::{format_duration, RunOutcome};
use crate::tui::theme::YggTheme;

use super::terminal_text::sanitize_for_terminal;
use super::{
    finish_transcript_block, fit_line, semantic_separator, subdued_text, wrap_hanging, OutcomeBlock,
};

const MAX_OUTCOME_DETAIL_BYTES: usize = 4 * 1024;

pub(super) fn completion_text(
    elapsed: Duration,
    separator: &str,
    tokens_per_second: Option<f64>,
) -> String {
    completion_status_text("completed", elapsed, separator, tokens_per_second)
}

pub(super) fn completion_with_warnings_text(
    elapsed: Duration,
    separator: &str,
    tokens_per_second: Option<f64>,
) -> String {
    completion_status_text(
        "completed with warnings",
        elapsed,
        separator,
        tokens_per_second,
    )
}

fn completion_status_text(
    status: &str,
    elapsed: Duration,
    separator: &str,
    tokens_per_second: Option<f64>,
) -> String {
    let mut text = format!("{status}{separator}{}", format_duration(elapsed));
    if let Some(rate) = tokens_per_second.filter(|rate| rate.is_finite() && *rate > 0.0) {
        text.push_str(&format!("{separator}{rate:.0} tok/s"));
    }
    text
}

fn outcome_line(outcome: &RunOutcome, tokens_per_second: Option<f64>, theme: &YggTheme) -> String {
    let separator = semantic_separator(theme);
    match outcome {
        RunOutcome::Completed { elapsed, .. } => {
            let text = subdued_text(
                theme,
                &completion_text(*elapsed, separator, tokens_per_second),
            );
            format!("{} {text}", theme.fg("success", theme.glyph("success")))
        }
        RunOutcome::CompletedWithWarnings { elapsed, .. } => {
            let text = subdued_text(
                theme,
                &completion_with_warnings_text(*elapsed, separator, tokens_per_second),
            );
            format!("{} {text}", theme.fg("warning", theme.glyph("warning")))
        }
        RunOutcome::Failed { elapsed, .. } => format!(
            "{} {}",
            theme.fg("error", theme.glyph("error")),
            theme.fg(
                "error",
                &format!("failed{separator}{}", format_duration(*elapsed))
            )
        ),
        RunOutcome::Interrupted { elapsed } => format!(
            "{} {}",
            theme.fg("warning", theme.glyph("interrupt")),
            subdued_text(
                theme,
                &format!("interrupted{separator}{}", format_duration(*elapsed)),
            )
        ),
        RunOutcome::NeedsInput { .. } => format!(
            "{} {}",
            theme.fg("warning", theme.glyph("note")),
            subdued_text(theme, "needs input")
        ),
    }
}

pub(super) fn bounded_outcome_detail(raw: &str) -> String {
    let mut safe = sanitize_for_terminal(raw);
    if safe.len() <= MAX_OUTCOME_DETAIL_BYTES {
        return safe;
    }

    let mut end = MAX_OUTCOME_DETAIL_BYTES - '…'.len_utf8();
    while end > 0 && !safe.is_char_boundary(end) {
        end -= 1;
    }
    safe.truncate(end);
    safe.push('…');
    safe
}

pub(super) fn render_outcome(outcome: &OutcomeBlock, theme: &YggTheme, width: u16) -> Vec<String> {
    let mut lines = vec![fit_line(
        &outcome_line(&outcome.outcome, outcome.tokens_per_second, theme),
        width,
    )];
    let detail = match &outcome.outcome {
        // Inference diagnostics are credential-redacted at the request boundary.
        // Bound and terminal-sanitize them again at this presentation boundary.
        RunOutcome::Failed { reason, .. } => Some(("error", reason.as_str())),
        RunOutcome::NeedsInput { prompt } => Some(("warning", prompt.as_str())),
        _ => None,
    };
    if let Some((role, detail)) = detail {
        let safe = bounded_outcome_detail(detail);
        for source_line in safe.split('\n') {
            if source_line.is_empty() {
                lines.push(String::new());
                continue;
            }
            lines.extend(wrap_hanging(
                &theme.fg(role, source_line),
                "  ",
                "  ",
                width,
            ));
        }
    }
    finish_transcript_block(lines)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sexy_tui_rs::strip_terminal_sequences;

    use super::super::{block_copy_text, OutcomeBlock, TranscriptBlock};
    use super::*;
    use crate::presentation::RunSummary;

    #[test]
    fn renderer_covers_all_terminal_outcomes() {
        let theme = crate::tui::theme::test_theme();
        let summary = RunSummary {
            files_changed: 2,
            tool_calls: 4,
            warnings: 0,
        };
        let outcomes = [
            (
                RunOutcome::Completed {
                    elapsed: Duration::from_millis(13700),
                    summary: summary.clone(),
                },
                "completed · 13.7s",
            ),
            (
                RunOutcome::CompletedWithWarnings {
                    elapsed: Duration::from_millis(18200),
                    warnings: 2,
                    summary: RunSummary {
                        warnings: 2,
                        ..summary.clone()
                    },
                },
                "completed with warnings · 18.2s",
            ),
            (
                RunOutcome::Failed {
                    elapsed: Duration::from_millis(9400),
                    reason: "command exited 1".into(),
                },
                "failed",
            ),
            (
                RunOutcome::Interrupted {
                    elapsed: Duration::from_millis(6800),
                },
                "interrupted · 6.8s",
            ),
            (
                RunOutcome::NeedsInput {
                    prompt: "choose an implementation".into(),
                },
                "needs input",
            ),
        ];
        for (outcome, expected) in outcomes {
            let rendered = outcome_line(&outcome, None, &theme);
            assert!(rendered.contains(expected), "{rendered:?}");
            if matches!(outcome, RunOutcome::CompletedWithWarnings { .. }) {
                assert!(
                    strip_terminal_sequences(&rendered).starts_with('◇'),
                    "completed-with-warnings should retain a warning signal: {rendered:?}"
                );
            }
            assert!(
                rendered.contains('✓')
                    || rendered.contains('◇')
                    || rendered.contains('×')
                    || rendered.contains('■')
            );
        }
    }

    #[test]
    fn warning_outcome_keeps_warning_status_and_final_throughput() {
        let theme = crate::tui::theme::test_theme();
        let outcome = RunOutcome::CompletedWithWarnings {
            elapsed: Duration::from_secs(25 * 60 + 31),
            warnings: 13,
            summary: RunSummary {
                files_changed: 0,
                tool_calls: 0,
                warnings: 13,
            },
        };

        assert_eq!(
            strip_terminal_sequences(&outcome_line(&outcome, Some(104.0), &theme)),
            "◇ completed with warnings · 25m31s · 104 tok/s"
        );
        assert_eq!(
            block_copy_text(&TranscriptBlock::Outcome(OutcomeBlock::new(
                outcome,
                Some(104.0),
            ))),
            "completed with warnings · 25m31s · 104 tok/s"
        );
    }

    #[test]
    fn failed_outcome_keeps_the_headline_and_shows_a_bounded_safe_reason() {
        let theme = crate::tui::theme::test_theme();
        let reason = format!(
            "\x1b[31mProvider unavailable\x1b[0m\x07\n{}",
            "é".repeat(MAX_OUTCOME_DETAIL_BYTES)
        );
        let outcome = RunOutcome::Failed {
            elapsed: Duration::from_millis(9400),
            reason,
        };

        assert_eq!(
            strip_terminal_sequences(&outcome_line(&outcome, None, &theme)),
            "× failed · 9.4s"
        );
        let RunOutcome::Failed { reason, .. } = &outcome else {
            unreachable!()
        };
        let detail = bounded_outcome_detail(reason);
        assert!(detail.starts_with("Provider unavailable␇\n"), "{detail:?}");
        assert!(detail.ends_with('…'));
        assert!(detail.len() <= MAX_OUTCOME_DETAIL_BYTES);
        assert!(detail.is_char_boundary(detail.len()));
        assert!(!detail.contains("\x1b[31m"));
        assert!(detail
            .chars()
            .all(|character| !character.is_control() || character == '\n'));

        let rendered = render_outcome(&OutcomeBlock::new(outcome.clone(), None), &theme, 48)
            .into_iter()
            .map(|line| strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.starts_with("× failed · 9.4s\n"), "{rendered:?}");
        assert!(rendered.contains("Provider unavailable␇"), "{rendered:?}");

        let copied = block_copy_text(&TranscriptBlock::Outcome(OutcomeBlock::new(outcome, None)));
        assert!(copied.starts_with("failed · 9.4s\nProvider unavailable␇\n"));
        assert!(copied.ends_with('…'));
    }
}
