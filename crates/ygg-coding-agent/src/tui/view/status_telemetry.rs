use std::time::{Duration, Instant};

use ygg_ai::Usage;

use super::terminal_text::sanitize_for_terminal;
use super::{PriceDisplay, ShellState, YggTheme};

/// Calculate a nonzero output-generation rate from a token count and measured
/// generation interval. Completed turns pass provider-reported tokens; live
/// rendering passes the explicitly marked character-based estimate.
pub(super) fn output_tokens_per_second(output_tokens: u64, elapsed: Duration) -> Option<f64> {
    (output_tokens > 0 && !elapsed.is_zero())
        .then(|| output_tokens as f64 / elapsed.as_secs_f64())
        .filter(|rate| rate.is_finite())
}

pub(super) fn usage_cache_hit_rate_basis_points(usage: Usage) -> Option<u16> {
    let prompt_tokens = usage
        .input_tokens
        .saturating_add(usage.cache_read_tokens)
        .saturating_add(usage.cache_write_tokens);
    if prompt_tokens == 0 || (usage.cache_read_tokens == 0 && usage.cache_write_tokens == 0) {
        return None;
    }
    Some(((u128::from(usage.cache_read_tokens) * 10_000) / u128::from(prompt_tokens)) as u16)
}

fn status_dollars(microdollars: u64) -> String {
    format!("${:.6}", microdollars as f64 / 1_000_000.0)
}

pub(super) fn status_telemetry(state: &ShellState, now: Instant) -> String {
    let mut lines = vec!["Telemetry".to_owned()];
    if let Some(usage) = state.last_turn_usage {
        lines.extend([
            "Usage source   provider-reported (exact)".to_owned(),
            format!("Input tokens   {}", usage.input_tokens),
            format!("Cache read     {}", usage.cache_read_tokens),
            format!("Cache write    {}", usage.cache_write_tokens),
            format!("Output tokens  {}", usage.output_tokens),
            format!("Reasoning      {}", usage.reasoning_tokens),
            format!("Total tokens   {}", usage.total_tokens),
        ]);
    } else if let Some(tokens) = state.live_generated_tokens() {
        lines.push(format!("Output tokens  ~{tokens} (stream estimate)"));
        lines.push("Usage source   awaiting provider report".to_owned());
    } else {
        lines.push("Usage source   unavailable (no completed model turn)".to_owned());
    }

    let active = state.run.current().is_some_and(|run| run.is_active());
    match state.price_display {
        PriceDisplay::Unknown => {
            lines.push("Turn cost      unavailable (pricing not configured)".to_owned());
            lines.push("Session cost   unavailable (pricing not configured)".to_owned());
        }
        PriceDisplay::ExplicitZero => {
            lines.push("Turn cost      $0 (configured zero-priced)".to_owned());
            lines.push("Session cost   $0 (configured zero-priced)".to_owned());
        }
        PriceDisplay::Priced => {
            if state.run_cost_available {
                let approximate = if active { "~" } else { "" };
                lines.push(format!(
                    "Turn cost      {approximate}{} ({})",
                    status_dollars(state.run_cost_microdollars),
                    if active { "incomplete" } else { "reported" }
                ));
            } else {
                lines.push("Turn cost      unavailable (no durable completed run)".to_owned());
            }
            lines.push(match state.session_cost_microdollars {
                Some(cost) => format!("Session cost   {} (reported)", status_dollars(cost)),
                None => "Session cost   awaiting first usage report".to_owned(),
            });
        }
    }

    if let (Some(rate), Some(tokens), Some(elapsed)) = (
        state.last_turn_tokens_per_second,
        state.last_turn_generated_tokens,
        state.last_turn_generation_elapsed,
    ) {
        lines.push(format!(
            "Throughput     {rate:.1} tok/s final ({tokens} reported tokens / {:.2}s measured)",
            elapsed.as_secs_f64()
        ));
    } else if let Some(started) = state.turn_generation_started_at {
        lines.push(format!(
            "Throughput     awaiting turn completion ({:.2}s generation in progress)",
            now.saturating_duration_since(started).as_secs_f64()
        ));
    } else {
        lines.push("Throughput     unavailable".to_owned());
    }
    lines.join("\n")
}

pub(super) fn styled_status_text(theme: &YggTheme, text: &str) -> String {
    let safe = sanitize_for_terminal(text);
    let mut metadata = true;
    safe.lines()
        .map(|line| {
            if line.is_empty() {
                metadata = false;
                return String::new();
            }
            if !metadata {
                return line.to_owned();
            }
            let Some(separator) = line.find("  ") else {
                return line.to_owned();
            };
            let label = &line[..separator];
            let spacing_and_value = &line[separator..];
            let spacing = spacing_and_value
                .chars()
                .take_while(|character| character.is_whitespace())
                .collect::<String>();
            let value = &spacing_and_value[spacing.len()..];
            let value = if label == "Model" {
                theme.bold(&theme.fg("model_accent", value))
            } else {
                value.to_owned()
            };
            format!("{}{}{}", theme.fg("model_accent", label), spacing, value)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::terminal::{ColorDepth, TerminalCapabilities};

    #[test]
    fn output_token_rate_uses_authoritative_usage_and_generation_elapsed_time() {
        assert_eq!(
            output_tokens_per_second(120, Duration::from_secs(2)),
            Some(60.0)
        );
        assert!(output_tokens_per_second(1, Duration::from_millis(250))
            .is_some_and(|rate| (rate - 4.0).abs() < f64::EPSILON));
        assert_eq!(output_tokens_per_second(0, Duration::from_secs(1)), None);
        assert_eq!(output_tokens_per_second(1, Duration::ZERO), None);
    }

    #[test]
    fn status_metadata_uses_the_model_accent_but_no_color_stays_plain() {
        let mut theme = crate::tui::theme::test_theme();
        crate::tui::theme::apply_model_lab(&mut theme, crate::tui::theme::ModelLab::Anthropic);
        let styled = styled_status_text(
            &theme,
            "Provider       anthropic\nModel          claude\nReasoning      high\n\nSecurity model: trusted local agent",
        );
        assert!(styled.contains("38;2;169;99;76"), "{styled:?}");
        assert!(styled.contains("Model"));
        assert!(styled.contains("claude"));

        let mut plain = crate::tui::theme::test_theme_with(TerminalCapabilities::test(
            true,
            true,
            ColorDepth::None,
        ));
        crate::tui::theme::apply_model_lab(&mut plain, crate::tui::theme::ModelLab::Anthropic);
        let plain = styled_status_text(&plain, "Model          claude");
        assert_eq!(plain, "Model          claude");
        assert!(!plain.contains('\x1b'));
    }
}

/// Chrome for extension slash-command output. The body is sanitized here, then
/// framed with a heading rule and light per-line styling so long extension
/// reports stay scannable: `label:` prefixes read as headings, `-` bullets get
/// a quiet marker, and `·` separators stay dim.
pub(super) fn styled_extension_output(theme: &YggTheme, command: &str, text: &str) -> String {
    let safe = sanitize_for_terminal(text);
    let rule_width = 28;
    let rule = theme.fg("muted", &theme.glyph("horizontal").repeat(rule_width));
    let heading = format!(
        "{} {}",
        theme.settled_event_dot("neutral", if theme.unicode() { "•" } else { "*" }),
        theme.bold(&theme.fg("foreground", &format!("/{command}")))
    );
    let mut lines = vec![rule.clone(), heading, rule.clone()];
    for line in safe.lines() {
        let styled_line = if let Some((label, rest)) = line.split_once(':') {
            if !label.is_empty()
                && label.len() <= 32
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_')
                && !rest.contains(':')
            {
                format!("{}{}{}", theme.fg("model_accent", label), ":", rest)
            } else {
                line.to_owned()
            }
        } else if let Some(rest) = line.strip_prefix("- ") {
            format!("{} {}", theme.fg("muted", "-"), rest)
        } else {
            line.to_owned()
        };
        lines.push(styled_line);
    }
    lines.push(rule);
    lines.join("\n")
}

#[cfg(test)]
mod extension_output_tests {
    use super::*;

    #[test]
    fn styled_extension_output_frames_and_labels_the_body() {
        let theme = crate::tui::theme::test_theme();
        let styled = styled_extension_output(
            &theme,
            "web-search",
            "provider: brave\n- result one\nplain line\nsome:thing: odd",
        );
        let plain = sanitize_for_terminal(&styled);
        assert!(plain.contains("/web-search"));
        assert!(plain.contains("provider: brave"));
        assert!(plain.contains("- result one"));
        // Styling must survive as trusted ANSI in the overlay text.
        assert!(styled.contains('\x1b'));
        // The body itself was sanitized; nothing raw leaks through.
        assert!(!styled.contains("\u{7}"));
    }

    #[test]
    fn styled_extension_output_is_plain_when_color_depth_is_none() {
        let theme =
            crate::tui::theme::test_theme_with(crate::tui::terminal::TerminalCapabilities::test(
                true,
                true,
                crate::tui::terminal::ColorDepth::None,
            ));
        let styled = styled_extension_output(&theme, "extensions", "no extensions configured");
        assert!(!styled.contains('\x1b'), "{styled:?}");
        assert!(styled.contains("/extensions"));
    }
}
