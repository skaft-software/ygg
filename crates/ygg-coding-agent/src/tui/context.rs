use ygg_agent::InputPart;

use crate::app::bootstrap::effective_compaction_threshold_fraction;
use crate::app::App;
use crate::presentation::ModelDisplayMetadata;
use crate::tui::theme::YggTheme;
use crate::tui::view::{fit_line, sanitize_for_terminal};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextKind {
    Runtime,
    Instructions,
    ToolResults,
    Conversation,
    Attachments,
    Summary,
    Other,
    Free,
    Buffer,
}

impl ContextKind {
    // Keep the existing context palette channels stable while the report moves
    // from transport-shaped estimates to the agent's semantic breakdown.
    fn role(self) -> &'static str {
        match self {
            Self::Runtime => "context_system",
            Self::Instructions => "context_framing",
            Self::ToolResults => "context_tools",
            Self::Conversation => "context_messages",
            Self::Attachments => "context_pending",
            Self::Summary => "context_adjustment",
            Self::Other => "context_tokenizer_adjustment",
            Self::Free => "context_free",
            Self::Buffer => "context_buffer",
        }
    }

    fn requires_visible_cell(self) -> bool {
        self != Self::Free
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ContextSlice {
    kind: ContextKind,
    label: String,
    tokens: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GridCell {
    kind: ContextKind,
    partial: bool,
}

#[derive(Clone, Copy, Debug)]
struct CellAllocation {
    cells: usize,
    minimum: usize,
    numerator: u128,
}

/// Request-context estimate captured at the instant `/context` is invoked.
/// It stores semantic quantities, not rendered rows, so resize and theme
/// changes can re-render the same report without stale colours or geometry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextReport {
    model_display: String,
    model: String,
    context_window: u64,
    estimated_input: u64,
    slices: Vec<ContextSlice>,
}

impl ContextReport {
    pub(crate) fn capture(app: &App, _pending: &[InputPart]) -> Self {
        let breakdown = app
            .agent
            .request_context_breakdown()
            .expect("an active application has a valid context branch");
        let context_window = breakdown.context_limit;
        let estimated_input = breakdown.total_tokens;
        let auto_compact_enabled = app.config.compaction.mode.enabled();
        let threshold_fraction = effective_compaction_threshold_fraction(&app.config, &app.model);
        let auto_compact_threshold = if auto_compact_enabled {
            ((context_window as f64) * threshold_fraction).floor() as u64
        } else {
            context_window
        };
        let free = auto_compact_threshold.saturating_sub(estimated_input);
        let buffer = if auto_compact_enabled {
            context_window.saturating_sub(auto_compact_threshold)
        } else {
            0
        };

        // ContextBreakdown is the single accounting source. The optional
        // buffer remains product-owned because it marks capacity deliberately
        // kept beyond the active auto-compaction window.
        let mut slices = vec![
            ContextSlice {
                kind: ContextKind::Runtime,
                label: "Runtime framing and tools".into(),
                tokens: breakdown.system_tokens,
            },
            ContextSlice {
                kind: ContextKind::Instructions,
                label: "System instructions".into(),
                tokens: breakdown.instruction_tokens,
            },
            ContextSlice {
                kind: ContextKind::ToolResults,
                label: "Tool calls and results".into(),
                tokens: breakdown.tool_result_tokens,
            },
            ContextSlice {
                kind: ContextKind::Conversation,
                label: "Conversation".into(),
                tokens: breakdown.conversation_tokens,
            },
            ContextSlice {
                kind: ContextKind::Attachments,
                label: "Attachments".into(),
                tokens: breakdown.attachment_tokens,
            },
            ContextSlice {
                kind: ContextKind::Summary,
                label: "Compaction summary".into(),
                tokens: breakdown.compaction_summary_tokens,
            },
            ContextSlice {
                kind: ContextKind::Other,
                label: "Provider/unattributed".into(),
                tokens: breakdown.other_tokens,
            },
            ContextSlice {
                kind: ContextKind::Free,
                label: "Free space".into(),
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

        let model_display = ModelDisplayMetadata::resolve(&app.model.spec).name;
        Self {
            model_display,
            model: app.model.spec.id.0.clone(),
            context_window,
            estimated_input,
            slices,
        }
    }

    pub(crate) fn render(&self, theme: &YggTheme, width: u16) -> Vec<String> {
        let width = width.max(1);
        let mut lines = vec![fit_line(&theme.bold("Context Usage"), width)];
        let (columns, _) = grid_dimensions(self.context_window);
        let spaced_grid = usize::from(width) >= columns.saturating_mul(2);
        let grid_width = if spaced_grid {
            columns.saturating_mul(2)
        } else {
            columns
        };
        let gap = 3usize;
        let side_by_side = usize::from(width) >= grid_width.saturating_add(gap + 52);
        let detail_width = if side_by_side {
            usize::from(width).saturating_sub(grid_width + gap).max(1) as u16
        } else {
            width
        };
        let details = self.render_details(theme, detail_width);
        let grid = self.render_grid(theme, width, spaced_grid);

        if side_by_side {
            let row_count = grid.len().max(details.len());
            for row in 0..row_count {
                let left = grid.get(row).map(String::as_str).unwrap_or("");
                let right = details.get(row).map(String::as_str).unwrap_or("");
                let left = pad_visible(left, grid_width);
                lines.push(fit_line(
                    &format!("{left}{}{right}", " ".repeat(gap)),
                    width,
                ));
            }
        } else {
            lines.extend(grid.into_iter().map(|line| fit_line(&line, width)));
            lines.push(String::new());
            lines.extend(details);
        }
        lines
    }

    fn render_details(&self, theme: &YggTheme, width: u16) -> Vec<String> {
        let percent = if self.context_window == 0 {
            0.0
        } else {
            self.estimated_input as f64 * 100.0 / self.context_window as f64
        };
        let mut lines = vec![
            fit_line(
                &theme.bold(&sanitize_for_terminal(&self.model_display)),
                width,
            ),
            fit_line(&sanitize_for_terminal(&self.model), width),
            fit_line(
                &format!(
                    "{}/{} tokens ({percent:.0}%)",
                    compact_tokens(self.estimated_input),
                    compact_tokens(self.context_window)
                ),
                width,
            ),
            String::new(),
            fit_line(&theme.fg("muted", "Estimated usage by category"), width),
        ];
        lines.extend(
            self.slices
                .iter()
                .filter(|slice| slice.tokens > 0)
                .map(|slice| render_slice(slice, theme, width, self.context_window)),
        );
        lines
    }

    fn render_grid(&self, theme: &YggTheme, width: u16, spaced: bool) -> Vec<String> {
        let (columns, _) = grid_dimensions(self.context_window);
        self.grid_cells()
            .chunks(columns)
            .map(|row| {
                let mut line = String::new();
                for (index, cell) in row.iter().enumerate() {
                    line.push_str(&render_grid_cell(*cell, theme));
                    if spaced && (index + 1 < row.len() || usize::from(width) >= columns * 2) {
                        line.push(' ');
                    }
                }
                line
            })
            .collect()
    }

    fn grid_cells(&self) -> Vec<GridCell> {
        let (columns, rows) = grid_dimensions(self.context_window);
        let cell_count = columns.saturating_mul(rows).max(1);
        let slice_total = self
            .slices
            .iter()
            .map(|slice| slice.tokens)
            .fold(0u64, u64::saturating_add);
        // Ordinarily the slices exactly fill the model window. Over-limit
        // reports normalize to their displayed total so no semantic category
        // is dropped merely because the request already crossed the limit.
        let denominator = u128::from(self.context_window.max(slice_total).max(1));
        let mut allocations = self
            .slices
            .iter()
            .map(|slice| {
                let numerator = u128::from(slice.tokens).saturating_mul(cell_count as u128);
                let proportional = (numerator / denominator) as usize;
                let minimum = usize::from(slice.tokens > 0 && slice.kind.requires_visible_cell());
                CellAllocation {
                    cells: proportional.max(minimum),
                    minimum,
                    numerator,
                }
            })
            .collect::<Vec<_>>();

        let mut allocated = allocations.iter().map(|item| item.cells).sum::<usize>();
        if allocated > cell_count {
            let mut excess = allocated - cell_count;
            if let Some((index, _)) = self
                .slices
                .iter()
                .enumerate()
                .find(|(_, slice)| slice.kind == ContextKind::Free)
            {
                let removable = allocations[index]
                    .cells
                    .saturating_sub(allocations[index].minimum)
                    .min(excess);
                allocations[index].cells -= removable;
                excess -= removable;
            }
            while excess > 0 {
                let Some(index) = allocations
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| item.cells > item.minimum)
                    .max_by_key(|(_, item)| {
                        (item.cells as u128)
                            .saturating_mul(denominator)
                            .saturating_sub(item.numerator)
                    })
                    .map(|(index, _)| index)
                else {
                    break;
                };
                allocations[index].cells -= 1;
                excess -= 1;
            }
            allocated = allocations.iter().map(|item| item.cells).sum();
        }

        while allocated < cell_count {
            let index = allocations
                .iter()
                .enumerate()
                .max_by_key(|(_, item)| {
                    item.numerator
                        .saturating_sub((item.cells as u128).saturating_mul(denominator))
                })
                .map(|(index, _)| index)
                .or_else(|| {
                    self.slices
                        .iter()
                        .position(|slice| slice.kind == ContextKind::Free)
                })
                .unwrap_or(0);
            allocations[index].cells += 1;
            allocated += 1;
        }

        let mut cells = Vec::with_capacity(cell_count);
        for (slice, allocation) in self.slices.iter().zip(allocations) {
            for index in 0..allocation.cells {
                let lower = (index as u128).saturating_mul(denominator);
                let upper = ((index + 1) as u128).saturating_mul(denominator);
                let partial = allocation.numerator > lower && allocation.numerator < upper;
                cells.push(GridCell {
                    kind: slice.kind,
                    partial,
                });
            }
        }
        debug_assert_eq!(cells.len(), cell_count);
        cells
    }
}

fn grid_dimensions(context_window: u64) -> (usize, usize) {
    // Physical area communicates model capacity: 128K is 8×8, the common
    // 200K/272K class is 10×10, and a 1M window expands to 20×10.
    if context_window <= 131_072 {
        (8, 8)
    } else if context_window <= 272_000 {
        (10, 10)
    } else if context_window < 1_000_000 {
        (16, 10)
    } else {
        (20, 10)
    }
}

fn render_grid_cell(cell: GridCell, theme: &YggTheme) -> String {
    let glyph = if theme.unicode() {
        match cell.kind {
            ContextKind::Free => "⛶",
            ContextKind::Buffer => "⛝",
            _ if cell.partial => "⛀",
            _ => "⛁",
        }
    } else {
        match cell.kind {
            ContextKind::Free => ".",
            ContextKind::Buffer => ":",
            _ if cell.partial => "+",
            _ => "#",
        }
    };
    theme.fg(cell.kind.role(), glyph)
}

fn render_slice(slice: &ContextSlice, theme: &YggTheme, width: u16, window: u64) -> String {
    let role = slice.kind.role();
    let marker = render_grid_cell(
        GridCell {
            kind: slice.kind,
            partial: false,
        },
        theme,
    );
    let label = theme.fg(role, &sanitize_for_terminal(&slice.label));
    let percent = if window == 0 {
        0.0
    } else {
        slice.tokens as f64 * 100.0 / window as f64
    };
    fit_line(
        &format!(
            "{marker} {label}: {} tokens ({percent:.1}%)",
            compact_tokens(slice.tokens)
        ),
        width,
    )
}

fn pad_visible(line: &str, width: usize) -> String {
    let padding = width.saturating_sub(sexy_tui_rs::visible_width(line));
    format!("{line}{}", " ".repeat(padding))
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

    fn report(context_window: u64) -> ContextReport {
        ContextReport {
            model_display: "Ornith 35B".into(),
            model: "custom/ornith-35b".into(),
            context_window,
            estimated_input: 133_000,
            slices: vec![
                ContextSlice {
                    kind: ContextKind::Runtime,
                    label: "Runtime framing and tools".into(),
                    tokens: 3_000,
                },
                ContextSlice {
                    kind: ContextKind::Instructions,
                    label: "System instructions".into(),
                    tokens: 100,
                },
                ContextSlice {
                    kind: ContextKind::ToolResults,
                    label: "Tool calls and results".into(),
                    tokens: 5_000,
                },
                ContextSlice {
                    kind: ContextKind::Conversation,
                    label: "Conversation".into(),
                    tokens: 47_000,
                },
                ContextSlice {
                    kind: ContextKind::Attachments,
                    label: "Attachments".into(),
                    tokens: 900,
                },
                ContextSlice {
                    kind: ContextKind::Summary,
                    label: "Compaction summary".into(),
                    tokens: 3_000,
                },
                ContextSlice {
                    kind: ContextKind::Other,
                    label: "Provider/unattributed".into(),
                    tokens: 74_000,
                },
                ContextSlice {
                    kind: ContextKind::Free,
                    label: "Free space".into(),
                    tokens: context_window.saturating_sub(163_000),
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
    fn context_window_scales_both_grid_dimensions_before_one_million() {
        let small = grid_dimensions(128_000);
        let medium = grid_dimensions(200_000);
        let large = grid_dimensions(1_000_000);
        assert_eq!(small, (8, 8));
        assert_eq!(medium, (10, 10));
        assert_eq!(large, (20, 10));
        assert!(large.0 > small.0);
        assert!(large.1 > small.1);
    }

    #[test]
    fn every_nonzero_semantic_category_gets_a_partial_cell_at_one_million() {
        let kinds = [
            ContextKind::Runtime,
            ContextKind::Instructions,
            ContextKind::ToolResults,
            ContextKind::Conversation,
            ContextKind::Attachments,
            ContextKind::Summary,
            ContextKind::Other,
            ContextKind::Buffer,
        ];
        let mut tiny = ContextReport {
            model_display: "Large".into(),
            model: "large".into(),
            context_window: 1_000_000,
            estimated_input: kinds.len() as u64,
            slices: kinds
                .iter()
                .map(|kind| ContextSlice {
                    kind: *kind,
                    label: format!("{kind:?}"),
                    tokens: 1,
                })
                .collect(),
        };
        tiny.slices.push(ContextSlice {
            kind: ContextKind::Free,
            label: "Free space".into(),
            tokens: 1_000_000 - kinds.len() as u64,
        });

        let cells = tiny.grid_cells();
        assert_eq!(cells.len(), 200);
        for kind in kinds {
            assert!(
                cells.iter().any(|cell| cell.kind == kind && cell.partial),
                "{kind:?} did not receive a partial cell"
            );
        }
    }

    #[test]
    fn context_grid_exactly_fills_its_context_sized_box() {
        for window in [128_000, 200_000, 872_000, 1_000_000] {
            let report = report(window);
            let (columns, rows) = grid_dimensions(window);
            assert_eq!(report.grid_cells().len(), columns * rows);
        }
    }

    #[test]
    fn wide_and_narrow_context_reports_keep_exact_rows_without_overflow() {
        let report = report(1_000_000);
        let theme = crate::tui::theme::test_theme();
        for width in [40, 72, 80, 100, 140] {
            let rendered = report.render(&theme, width);
            let plain = rendered
                .iter()
                .map(|line| sexy_tui_rs::strip_terminal_sequences(line))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(plain.contains("Context Usage"), "{plain}");
            assert!(plain.contains("Ornith 35B"), "{plain}");
            assert!(plain.contains("Runtime framing and tools"), "{plain}");
            assert!(plain.contains("Tool calls and results"), "{plain}");
            assert!(plain.contains("Conversation"), "{plain}");
            assert!(plain.contains("Provider/unattributed"), "{plain}");
            assert!(plain.contains("Auto-compact buffer"), "{plain}");
            if width >= 80 {
                assert!(
                    plain.contains("Runtime framing and tools: 3.0k tokens (0.3%)"),
                    "{plain}"
                );
            }
            assert!(rendered
                .iter()
                .all(|line| sexy_tui_rs::visible_width(line) <= usize::from(width)));
        }
    }

    #[test]
    fn context_report_restyles_from_semantics_for_each_theme() {
        let report = report(200_000);
        let default = report
            .render(&crate::tui::theme::test_theme(), 100)
            .join("\n");
        let named = report
            .render(
                &crate::tui::theme::test_bundled_theme_with(
                    "clawed",
                    crate::tui::terminal::TerminalCapabilities::test(
                        true,
                        true,
                        crate::tui::terminal::ColorDepth::TrueColor,
                    ),
                    crate::tui::theme::TerminalBackground::Dark,
                ),
                100,
            )
            .join("\n");
        assert_eq!(
            sexy_tui_rs::strip_terminal_sequences(&default),
            sexy_tui_rs::strip_terminal_sequences(&named)
        );
        assert_ne!(default, named, "theme semantics did not restyle the report");
    }

    #[test]
    fn context_categories_keep_distinct_colour_channels() {
        let theme = crate::tui::theme::test_theme();
        let kinds = [
            ContextKind::Runtime,
            ContextKind::Instructions,
            ContextKind::ToolResults,
            ContextKind::Conversation,
            ContextKind::Attachments,
            ContextKind::Summary,
            ContextKind::Other,
            ContextKind::Free,
            ContextKind::Buffer,
        ];
        let colors = kinds
            .iter()
            .map(|kind| theme.role_rgb(kind.role()))
            .collect::<Vec<_>>();
        for (index, color) in colors.iter().enumerate() {
            for other in colors.iter().skip(index + 1) {
                assert_ne!(color, other, "context color channel was reused");
            }
        }
    }

    #[test]
    fn context_report_uses_agent_context_breakdown() {
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

        let breakdown = app.agent.request_context_breakdown().unwrap();
        assert_eq!(breakdown.total_tokens, 133_000);
        assert!(breakdown.conversation_tokens > 0);
        assert!(breakdown.other_tokens > 0);

        let report = ContextReport::capture(&app, &[]);
        assert_eq!(report.estimated_input, breakdown.total_tokens);
        for (kind, tokens) in [
            (ContextKind::Runtime, breakdown.system_tokens),
            (ContextKind::Instructions, breakdown.instruction_tokens),
            (ContextKind::ToolResults, breakdown.tool_result_tokens),
            (ContextKind::Conversation, breakdown.conversation_tokens),
            (ContextKind::Attachments, breakdown.attachment_tokens),
            (ContextKind::Summary, breakdown.compaction_summary_tokens),
            (ContextKind::Other, breakdown.other_tokens),
        ] {
            assert_eq!(
                report
                    .slices
                    .iter()
                    .find(|slice| slice.kind == kind)
                    .map(|slice| slice.tokens),
                Some(tokens)
            );
        }
    }
}
