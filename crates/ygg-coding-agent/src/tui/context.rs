use ygg_agent::InputPart;

use crate::app::App;
use crate::compaction::{
    context_window, estimate_messages_tokens, estimate_next_request_tokens,
    estimate_pending_tokens, estimate_text_tokens,
};
use crate::tui::theme::YggTheme;
use crate::tui::view::{fit_line, sanitize_for_terminal};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextKind {
    System,
    Skills,
    Tools,
    Messages,
    Pending,
    Framing,
    Adjustment,
    TokenizerAdjustment,
    Output,
    Free,
    Buffer,
}

impl ContextKind {
    fn role(self) -> &'static str {
        match self {
            Self::System => "model_accent",
            Self::Skills => "warning",
            Self::Tools => "success",
            Self::Messages => "foreground",
            Self::Pending => "warning",
            Self::Framing | Self::Buffer => "muted",
            Self::Adjustment | Self::TokenizerAdjustment => "warning",
            Self::Output => "error",
            Self::Free => "success",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContextSlice {
    kind: ContextKind,
    label: String,
    tokens: u64,
}

/// Request-context estimate captured at the instant `/context` is invoked.
/// It stores semantic quantities, not rendered rows, so resize and theme
/// changes can re-render the same report without stale colours or geometry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextReport {
    model: String,
    context_window: u64,
    estimated_input: u64,
    output_reserve: u64,
    auto_compact_enabled: bool,
    auto_compact_threshold: u64,
    keep_recent_turns: usize,
    slices: Vec<ContextSlice>,
}

impl ContextReport {
    pub(crate) fn capture(app: &App, pending: &[InputPart]) -> Self {
        let session = app.agent.session();
        let messages = session
            .context_ref()
            .map(|messages| estimate_messages_tokens(&messages))
            .unwrap_or_default();
        let active = session
            .head_ref()
            .and_then(|head| session.resolve_active_skills(head).ok())
            .unwrap_or_default();
        let skills = active
            .active_skills
            .iter()
            .map(|skill| estimate_text_tokens(&skill.instructions))
            .sum::<u64>();
        let pending_tokens = estimate_pending_tokens(pending);
        let structural_fallback = estimate_next_request_tokens(app, &[]);
        let pending_delta =
            estimate_next_request_tokens(app, pending).saturating_sub(structural_fallback);
        let estimate = app.agent.request_context_estimate().ok();
        let structural_input = estimate
            .map(|estimate| estimate.structural_tokens)
            .unwrap_or(structural_fallback)
            .saturating_add(pending_delta);
        let estimated_input = estimate
            .map(|estimate| estimate.input_tokens)
            .unwrap_or(structural_fallback)
            .saturating_add(pending_delta);
        let known_input = app
            .system_tokens
            .saturating_add(skills)
            .saturating_add(app.tool_schema_tokens)
            .saturating_add(messages)
            .saturating_add(pending_tokens);
        let framing = structural_input.saturating_sub(known_input);
        let categorized_input = known_input.saturating_add(framing);
        let provider_adjustment = estimated_input.saturating_sub(categorized_input);
        let tokenizer_adjustment = categorized_input.saturating_sub(estimated_input);
        let context_window = context_window(&app.model);
        let output_reserve = app.agent.max_output_tokens();
        let (auto_compact_enabled, threshold_fraction, keep_recent_turns) =
            app.agent.compaction_policy();
        let auto_compact_threshold = if auto_compact_enabled {
            ((context_window as f64) * threshold_fraction).floor() as u64
        } else {
            context_window
        };
        let free =
            auto_compact_threshold.saturating_sub(estimated_input.saturating_add(output_reserve));
        let buffer = if auto_compact_enabled {
            context_window.saturating_sub(auto_compact_threshold)
        } else {
            0
        };
        let skill_label = match (active.active_skills.len(), active.skill_resources.len()) {
            (0, 0) => "Active skills/resources".to_owned(),
            (skills, resources) => format!("Skills/resources ({skills}/{resources})"),
        };

        let mut slices = vec![
            ContextSlice {
                kind: ContextKind::System,
                label: "System prompt".into(),
                tokens: app.system_tokens,
            },
            ContextSlice {
                kind: ContextKind::Skills,
                label: skill_label,
                tokens: skills,
            },
            ContextSlice {
                kind: ContextKind::Tools,
                label: "Tool schemas".into(),
                tokens: app.tool_schema_tokens,
            },
            ContextSlice {
                kind: ContextKind::Messages,
                // Loaded skill-resource bodies enter provider context through
                // their durable tool-result messages, so they are counted here
                // once rather than invented as a second context category.
                label: "Messages and tool results".into(),
                tokens: messages,
            },
            ContextSlice {
                kind: ContextKind::Pending,
                label: "Pending input".into(),
                tokens: pending_tokens,
            },
            ContextSlice {
                kind: ContextKind::Framing,
                label: "Provider framing".into(),
                tokens: framing,
            },
            ContextSlice {
                kind: ContextKind::Adjustment,
                label: "Provider-measured adjustment".into(),
                tokens: provider_adjustment,
            },
            ContextSlice {
                kind: ContextKind::TokenizerAdjustment,
                label: "Tokenizer adjustment".into(),
                tokens: tokenizer_adjustment,
            },
            ContextSlice {
                kind: ContextKind::Output,
                label: "Output reserve".into(),
                tokens: output_reserve,
            },
            ContextSlice {
                kind: ContextKind::Free,
                label: if auto_compact_enabled {
                    "Free before auto-compact".into()
                } else {
                    "Free space".into()
                },
                tokens: free,
            },
        ];
        if auto_compact_enabled {
            slices.push(ContextSlice {
                kind: ContextKind::Buffer,
                label: "Auto-compact buffer".into(),
                tokens: buffer,
            });
        }

        Self {
            model: app.model.spec.id.0.clone(),
            context_window,
            estimated_input,
            output_reserve,
            auto_compact_enabled,
            auto_compact_threshold,
            keep_recent_turns,
            slices,
        }
    }

    pub(crate) fn render(&self, theme: &YggTheme, width: u16) -> Vec<String> {
        let width = width.max(1);
        let mut lines = vec![fit_line(
            &theme.bold(&format!("Context · {}", sanitize_for_terminal(&self.model))),
            width,
        )];
        let committed = self.estimated_input.saturating_add(self.output_reserve);
        lines.push(fit_line(
            &format!(
                "~{} input + {} output reserve / {}",
                compact_tokens(self.estimated_input),
                compact_tokens(self.output_reserve),
                compact_tokens(self.context_window)
            ),
            width,
        ));
        let policy = if self.auto_compact_enabled {
            format!(
                "auto-compact at {} ({:.0}%) · keeps {} recent turn{}",
                compact_tokens(self.auto_compact_threshold),
                if self.context_window == 0 {
                    0.0
                } else {
                    self.auto_compact_threshold as f64 * 100.0 / self.context_window as f64
                },
                self.keep_recent_turns,
                if self.keep_recent_turns == 1 { "" } else { "s" }
            )
        } else {
            "auto-compact off".to_owned()
        };
        lines.push(fit_line(&theme.fg("muted", &policy), width));
        if committed > self.auto_compact_threshold {
            lines.push(fit_line(
                &theme.fg(
                    "warning",
                    &format!(
                        "~{} over the current compaction line",
                        compact_tokens(committed - self.auto_compact_threshold)
                    ),
                ),
                width,
            ));
        }
        lines.push(String::new());
        lines.push(self.render_bar(theme, width));
        lines.push(String::new());

        let visible = self
            .slices
            .iter()
            .filter(|slice| slice.tokens > 0 || slice.kind == ContextKind::Pending)
            .collect::<Vec<_>>();
        if width >= 86 {
            let cell_width = width.saturating_sub(3) / 2;
            for pair in visible.chunks(2) {
                let left = render_slice(pair[0], theme, cell_width, self.context_window);
                let right = pair
                    .get(1)
                    .map(|slice| render_slice(slice, theme, cell_width, self.context_window))
                    .unwrap_or_default();
                let gap = usize::from(width).saturating_sub(
                    sexy_tui_rs::visible_width(&left) + sexy_tui_rs::visible_width(&right),
                );
                lines.push(fit_line(
                    &format!("{left}{}{right}", " ".repeat(gap)),
                    width,
                ));
            }
        } else {
            lines.extend(
                visible
                    .into_iter()
                    .map(|slice| render_slice(slice, theme, width, self.context_window)),
            );
        }
        lines
    }

    fn render_bar(&self, theme: &YggTheme, width: u16) -> String {
        let bar_width = usize::from(width.saturating_sub(2)).clamp(1, 72);
        let glyph = if theme.unicode() { "━" } else { "=" };
        let mut used_cells = 0usize;
        let mut cumulative = 0u64;
        let mut bar = String::new();
        for slice in self
            .slices
            .iter()
            .filter(|slice| slice.tokens > 0 && slice.kind != ContextKind::TokenizerAdjustment)
        {
            cumulative = cumulative.saturating_add(slice.tokens);
            let boundary = if self.context_window == 0 {
                bar_width
            } else {
                ((u128::from(cumulative.min(self.context_window)) * bar_width as u128)
                    / u128::from(self.context_window)) as usize
            };
            let cells = boundary.saturating_sub(used_cells);
            if cells > 0 {
                bar.push_str(&theme.fg(slice.kind.role(), &glyph.repeat(cells)));
                used_cells = boundary;
            }
            if used_cells == bar_width {
                break;
            }
        }
        if used_cells < bar_width {
            bar.push_str(&theme.fg("muted", &glyph.repeat(bar_width - used_cells)));
        }
        fit_line(&bar, width)
    }
}

fn render_slice(slice: &ContextSlice, theme: &YggTheme, width: u16, window: u64) -> String {
    let marker = theme.fg(slice.kind.role(), if theme.unicode() { "■" } else { "#" });
    let percent = if window == 0 {
        0.0
    } else {
        slice.tokens as f64 * 100.0 / window as f64
    };
    let tokens = if slice.kind == ContextKind::TokenizerAdjustment {
        format!("−{}", compact_tokens(slice.tokens))
    } else {
        compact_tokens(slice.tokens)
    };
    fit_line(
        &format!("{marker} {}  {}  {percent:.1}%", slice.label, tokens),
        width,
    )
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> ContextReport {
        ContextReport {
            model: "custom/ornith-35b".into(),
            context_window: 200_000,
            estimated_input: 133_000,
            output_reserve: 32_000,
            auto_compact_enabled: true,
            auto_compact_threshold: 170_000,
            keep_recent_turns: 4,
            slices: vec![
                ContextSlice {
                    kind: ContextKind::System,
                    label: "System prompt".into(),
                    tokens: 3_000,
                },
                ContextSlice {
                    kind: ContextKind::Skills,
                    label: "Skills/resources (2/1)".into(),
                    tokens: 2_000,
                },
                ContextSlice {
                    kind: ContextKind::Tools,
                    label: "Tool schemas".into(),
                    tokens: 5_000,
                },
                ContextSlice {
                    kind: ContextKind::Messages,
                    label: "Messages and tool results".into(),
                    tokens: 47_000,
                },
                ContextSlice {
                    kind: ContextKind::Pending,
                    label: "Pending input".into(),
                    tokens: 900,
                },
                ContextSlice {
                    kind: ContextKind::Framing,
                    label: "Provider framing".into(),
                    tokens: 100,
                },
                ContextSlice {
                    kind: ContextKind::Adjustment,
                    label: "Provider-measured adjustment".into(),
                    tokens: 75_000,
                },
                ContextSlice {
                    kind: ContextKind::Output,
                    label: "Output reserve".into(),
                    tokens: 32_000,
                },
                ContextSlice {
                    kind: ContextKind::Free,
                    label: "Free before auto-compact".into(),
                    tokens: 5_000,
                },
                ContextSlice {
                    kind: ContextKind::Buffer,
                    label: "Auto-compact buffer".into(),
                    tokens: 30_000,
                },
            ],
        }
    }

    #[test]
    fn wide_and_narrow_context_reports_keep_honest_categories_without_overflow() {
        let report = report();
        let input_additions = report
            .slices
            .iter()
            .filter(|slice| {
                matches!(
                    slice.kind,
                    ContextKind::System
                        | ContextKind::Skills
                        | ContextKind::Tools
                        | ContextKind::Messages
                        | ContextKind::Pending
                        | ContextKind::Framing
                        | ContextKind::Adjustment
                )
            })
            .map(|slice| slice.tokens)
            .sum::<u64>();
        let tokenizer_adjustment = report
            .slices
            .iter()
            .find(|slice| slice.kind == ContextKind::TokenizerAdjustment)
            .map_or(0, |slice| slice.tokens);
        assert!(
            input_additions.saturating_sub(tokenizer_adjustment) <= report.estimated_input,
            "input categories exceed the displayed conservative input"
        );
        let theme = crate::tui::theme::test_theme();
        for width in [40, 72, 100] {
            let rendered = report.render(&theme, width);
            let plain = rendered
                .iter()
                .map(|line| sexy_tui_rs::strip_terminal_sequences(line))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(plain.contains("System prompt"), "{plain}");
            assert!(plain.contains("Skills/resources"), "{plain}");
            assert!(plain.contains("Tool schemas"), "{plain}");
            assert!(plain.contains("Messages and tool results"), "{plain}");
            assert!(plain.contains("Provider-measured adjustment"), "{plain}");
            assert!(plain.contains("Output reserve"), "{plain}");
            assert!(plain.contains("Auto-compact buffer"), "{plain}");
            assert!(!plain.contains("MCP"), "{plain}");
            assert!(rendered
                .iter()
                .all(|line| sexy_tui_rs::visible_width(line) <= usize::from(width)));
        }
    }

    #[test]
    fn context_report_restyles_from_semantics_for_each_theme() {
        let report = report();
        let default = report
            .render(&crate::tui::theme::test_theme(), 72)
            .join("\n");
        let named = report
            .render(
                &crate::tui::theme::test_bundled_theme_with(
                    "bone-machine",
                    crate::tui::terminal::TerminalCapabilities::test(
                        true,
                        true,
                        crate::tui::terminal::ColorDepth::TrueColor,
                    ),
                    crate::tui::theme::TerminalBackground::Dark,
                ),
                72,
            )
            .join("\n");
        assert_eq!(
            sexy_tui_rs::strip_terminal_sequences(&default),
            sexy_tui_rs::strip_terminal_sequences(&named)
        );
        assert_ne!(default, named, "theme semantics did not restyle the report");
    }

    #[test]
    fn context_report_uses_provider_measurement_when_structural_estimate_is_low() {
        use ygg_agent::EntryValue;
        use ygg_ai::{
            AssistantMessage, AssistantPart, Message, Protocol, Usage, UserMessage, UserPart,
        };

        let (_directory, mut app) = crate::compaction::tests::app_for_estimate();
        app.agent
            .session_mut()
            .append(EntryValue::Message(Message::User(UserMessage {
                content: vec![UserPart::Text("x".repeat(232_000))],
            })))
            .unwrap();
        let assistant = app
            .agent
            .session_mut()
            .append(EntryValue::Message(Message::Assistant(AssistantMessage {
                content: vec![AssistantPart::Text("done".into())],
                model: app.model.spec.id.clone(),
                protocol: Protocol::OpenAiChat,
            })))
            .unwrap();
        app.agent
            .session_mut()
            .record_assistant_usage(
                assistant,
                app.model.endpoint.id.clone(),
                app.model.spec.id.clone(),
                Usage {
                    total_tokens: 133_000,
                    ..Usage::default()
                },
                None,
            )
            .unwrap();

        let estimate = app.agent.request_context_estimate().unwrap();
        assert!(
            (57_000..=60_000).contains(&estimate.structural_tokens),
            "unexpected structural fixture: {estimate:?}"
        );
        assert_eq!(estimate.provider_tokens, Some(133_000));
        assert_eq!(estimate.input_tokens, 133_000);

        let report = ContextReport::capture(&app, &[]);
        assert_eq!(report.estimated_input, 133_000);
        let adjustment = report
            .slices
            .iter()
            .find(|slice| slice.kind == ContextKind::Adjustment)
            .expect("explicit provider adjustment");
        let categorized = report
            .slices
            .iter()
            .filter(|slice| {
                matches!(
                    slice.kind,
                    ContextKind::System
                        | ContextKind::Skills
                        | ContextKind::Tools
                        | ContextKind::Messages
                        | ContextKind::Pending
                        | ContextKind::Framing
                )
            })
            .map(|slice| slice.tokens)
            .sum::<u64>();
        assert_eq!(adjustment.tokens, 133_000u64.saturating_sub(categorized));
        let plain = report
            .render(&crate::tui::theme::test_theme(), 100)
            .into_iter()
            .map(|line| sexy_tui_rs::strip_terminal_sequences(&line))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("~133.0k input"), "{plain}");
        assert!(plain.contains("Provider-measured adjustment"), "{plain}");
    }
}
