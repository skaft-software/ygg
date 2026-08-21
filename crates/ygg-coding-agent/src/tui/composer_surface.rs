//! Composer surface: a clean multiline input area framed by top and bottom
//! rules, with a stable status footer below and quiet, model-adaptive colour.

use std::time::Instant;

use sexy_tui_rs::{visible_width, CURSOR_MARKER};

use crate::tui::view::fit_line;

fn composer_cursor_marker(state: &super::view::ShellState) -> &'static str {
    if state.panel.is_some() {
        ""
    } else {
        CURSOR_MARKER
    }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Internal text rows: starts at one line, grows with content up to a cap.
/// When the editor has more lines than the cap, an overflow indicator is shown.
///
/// `visual_lines` is the number of wrapped editor lines that would be needed
/// at the current terminal width, so long wrapping lines are counted properly.
pub fn composer_content_rows(terminal_rows: u16, visual_lines: usize) -> usize {
    let term = terminal_rows.max(3) as usize;
    // Scale the composer height with the terminal so zooming in/out
    // (Cmd +/-) naturally shows more or fewer prompt lines.
    let max_rows = if term >= 40 {
        (term / 5).clamp(4, 14)
    } else if term >= 28 {
        (term / 4).clamp(3, 10)
    } else if term >= 18 {
        5
    } else if term >= 10 {
        3
    } else {
        2
    };
    visual_lines.max(1).min(max_rows)
}

/// When the editor has more lines than visible, this many rows are hidden
/// and we show an overflow indicator.
#[allow(dead_code)]
pub fn composer_overflow_count(editor_lines: usize, visible_rows: usize) -> usize {
    editor_lines.saturating_sub(visible_rows)
}

// ---------------------------------------------------------------------------
// Bordered composer box
// ---------------------------------------------------------------------------

fn unicode(state: &super::view::ShellState) -> bool {
    state.theme.unicode()
}

fn horiz(state: &super::view::ShellState) -> &str {
    state.theme.glyph("horizontal")
}

// ---------------------------------------------------------------------------
// Unified composer frame
// ---------------------------------------------------------------------------

/// Render the entire composer: a top rule, full-width content rows, and a
/// bottom rule. The rules are plain horizontal lines with no corners, and
/// content rows carry no side borders, so text selected from the prompt
/// copies without border characters. The rules remain byte-stable while work
/// runs; live motion belongs to the semantic transcript entry doing the work.
fn render_composer_box(
    state: &super::view::ShellState,
    width: u16,
    _now: Instant,
    content_rows: usize,
) -> Vec<String> {
    let w = usize::from(width);
    if w < 4 {
        return render_plain_content(state, width);
    }

    let theme = &state.theme;
    let h = horiz(state);

    // The rules identify the selected/executing model without pretending
    // to measure progress. Idle chrome stays quiet; focus and work use the
    // captured run accent as one stable colour.
    let run_active = state.run.is_active();
    let accent = if run_active {
        theme.model_rgb(state.run_model_lab)
    } else {
        theme.role_rgb("model_accent")
    }
    .unwrap_or((128, 128, 128));
    // Keep the resting rules close to the terminal background. On a light
    // profile this moves toward white rather than turning into a black box;
    // focused input and active work use the model accent.
    let idle_border = theme.composer_idle_rgb(accent);
    let focused =
        state.panel.is_none() && (!state.editor.is_empty() || state.tool_input_prompt.is_some());
    let compacting = state.run_label == "compacting";
    let border_rgb = if focused || run_active || compacting {
        accent
    } else {
        idle_border
    };
    // Plain full-width rules, with no corner glyphs, so nothing in the
    // composer row carries a border character into copied text.
    let render_rule = || -> String { theme.rgb_fg(border_rgb, &h.repeat(w)) };

    let mut lines = Vec::with_capacity(content_rows + 2);

    // ---- top rule ----
    lines.push(render_rule());

    // ---- content rows ----
    let marker = theme.bold(&theme.model_fg(
        if run_active {
            state.run_model_lab
        } else {
            state.model_lab
        },
        theme.glyph("prompt"),
    ));
    // The APC marker occupies no display cell. sexy-tui removes it after
    // layout, positions the terminal cursor there, and the backend requests a
    // steady block shape. Inserting a beam glyph here would shift the text.
    let cursor_marker = composer_cursor_marker(state);
    // Rows span the full width between the rules; nothing else frames the
    // prompt, so selected text copies without border characters.
    let render_row = |content: &str| -> String {
        let content_width = visible_width(content);
        if content_width > w {
            fit_line(content, width)
        } else {
            format!("{content}{}", " ".repeat(w.saturating_sub(content_width)))
        }
    };

    let (editor, editor_cursor) = if let Some(prompt) = &state.tool_input_prompt {
        (prompt.clone(), prompt.len())
    } else {
        super::view::sanitized_editor(&state.editor, state.editor_cursor)
    };

    if editor.is_empty() {
        for i in 0..content_rows {
            if i == 0 {
                lines.push(render_row(&format!("{marker} {cursor_marker}")));
            } else {
                lines.push(render_row(""));
            }
        }
    } else {
        let layout =
            state.cached_editor_layout((w as u16).max(2), Some(&editor), Some(editor_cursor));
        let total_lines = layout.lines.len();
        let overflow = total_lines.saturating_sub(content_rows);
        let visible_rows = if overflow > 0 {
            (content_rows.saturating_sub(1)).max(1).min(total_lines)
        } else {
            content_rows.max(1).min(total_lines)
        };
        let mut start = layout
            .cursor_row
            .saturating_add(1)
            .saturating_sub(visible_rows);
        let end = (start + visible_rows).min(total_lines);
        if end.saturating_sub(start) < visible_rows {
            start = end.saturating_sub(visible_rows);
        }
        let hidden_above = start;
        let hidden_below = total_lines.saturating_sub(end);

        let mut rendered = Vec::with_capacity(content_rows);

        if hidden_above > 0 {
            let ellipsis = theme.glyph("ellipsis");
            let msg = format!(
                "{ellipsis} {hidden_above} more line{} above",
                if hidden_above == 1 { "" } else { "s" }
            );
            rendered.push(render_row(&theme.fg("model_accent", &msg)));
        }

        for index in start..end {
            let vis_line = &layout.lines[index];
            let content = if index == layout.cursor_row {
                let cursor = editor_cursor.clamp(vis_line.start, vis_line.visible_end);
                format!(
                    "{}{cursor_marker}{}",
                    &editor[vis_line.start..cursor],
                    &editor[cursor..vis_line.visible_end]
                )
            } else {
                editor[vis_line.start..vis_line.visible_end].to_owned()
            };
            let prefix = if index == 0 {
                format!("{marker} ")
            } else {
                "  ".to_owned()
            };
            rendered.push(render_row(&format!("{prefix}{content}")));
        }

        if hidden_below > 0 {
            let ellipsis = theme.glyph("ellipsis");
            let msg = format!(
                "{ellipsis} {hidden_below} more line{} below",
                if hidden_below == 1 { "" } else { "s" }
            );
            rendered.push(render_row(&theme.fg("model_accent", &msg)));
        }

        while rendered.len() < content_rows {
            rendered.push(render_row(""));
        }

        lines.append(&mut rendered);
    }

    // ---- bottom rule ----
    lines.push(render_rule());

    lines
}

// ---------------------------------------------------------------------------
// Plain content fallback (very narrow terminals)
// ---------------------------------------------------------------------------

fn render_plain_content(state: &super::view::ShellState, width: u16) -> Vec<String> {
    let marker = state
        .theme
        .bold(&state.theme.fg("model_accent", state.theme.glyph("prompt")));
    let cursor_marker = composer_cursor_marker(state);
    let (editor, editor_cursor) = if let Some(prompt) = &state.tool_input_prompt {
        (prompt.clone(), prompt.len())
    } else {
        super::view::sanitized_editor(&state.editor, state.editor_cursor)
    };
    if editor.is_empty() {
        return vec![format!("{marker} {cursor_marker}")];
    }
    let cursor = editor_cursor.min(editor.len());
    let line = format!(
        "{marker} {}{cursor_marker}{}",
        &editor[..cursor],
        &editor[cursor..]
    );
    vec![fit_line(&line, width)]
}

// ---------------------------------------------------------------------------
// Status footer (below the composer box)
// ---------------------------------------------------------------------------

/// Semantic footer group. Variants are ordered from most descriptive to
/// most compact; groups disappear as units rather than being byte-truncated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterKind {
    Identity,
    Tokens,
    CacheHit,
    Context,
    Cost,
    Activity,
}

struct FooterSegment {
    kind: FooterKind,
    variants: Vec<String>,
    variant: usize,
    visible: bool,
}

impl FooterSegment {
    fn new(kind: FooterKind, variants: Vec<String>) -> Self {
        Self {
            kind,
            variants,
            variant: 0,
            visible: true,
        }
    }

    fn text(&self) -> &str {
        &self.variants[self.variant.min(self.variants.len().saturating_sub(1))]
    }

    fn compact_once(&mut self) {
        if self.variant + 1 < self.variants.len() {
            self.variant += 1;
        }
    }
}

fn footer_width(segments: &[FooterSegment], gap: usize) -> usize {
    let visible = segments.iter().filter(|segment| segment.visible);
    let count = visible.clone().count();
    visible
        .map(|segment| visible_width(segment.text()))
        .sum::<usize>()
        + count.saturating_sub(1) * gap
}

fn hide_footer_kind(segments: &mut [FooterSegment], kind: FooterKind) {
    if let Some(segment) = segments.iter_mut().find(|segment| segment.kind == kind) {
        segment.visible = false;
    }
}

fn compact_footer_kind(segments: &mut [FooterSegment], kind: FooterKind) {
    if let Some(segment) = segments.iter_mut().find(|segment| segment.kind == kind) {
        segment.compact_once();
    }
}

pub(crate) fn format_microdollars(microdollars: u64) -> String {
    const MICRODOLLARS_PER_DOLLAR: u128 = 1_000_000;
    const SIGNIFICANT_FIGURES: i32 = 3;

    let microdollars = u128::from(microdollars);
    if microdollars == 0 {
        return "$0.000".to_owned();
    }

    let whole = microdollars / MICRODOLLARS_PER_DOLLAR;
    let exponent = if whole > 0 {
        whole.ilog10() as i32
    } else {
        microdollars.ilog10() as i32 - 6
    };
    let decimal_places = SIGNIFICANT_FIGURES - 1 - exponent;
    let rounding_power = 6 - decimal_places;
    let rounding_unit = if rounding_power > 0 {
        10u128.pow(rounding_power as u32)
    } else {
        1
    };
    let quotient = microdollars / rounding_unit;
    let remainder = microdollars % rounding_unit;
    let rounded = (quotient
        + if remainder >= rounding_unit.div_ceil(2) {
            1
        } else {
            0
        })
        * rounding_unit;

    // A carry can change the number of digits, e.g. 9.999 becomes 10.0.
    let rounded_whole = rounded / MICRODOLLARS_PER_DOLLAR;
    let rounded_exponent = if rounded_whole > 0 {
        rounded_whole.ilog10() as i32
    } else {
        rounded.ilog10() as i32 - 6
    };
    let decimal_places = SIGNIFICANT_FIGURES - 1 - rounded_exponent;
    if decimal_places <= 0 {
        return format!("${rounded_whole}");
    }

    let decimal_places = decimal_places as u32;
    let fraction = rounded % MICRODOLLARS_PER_DOLLAR;
    let fraction = if decimal_places <= 6 {
        fraction / 10u128.pow(6 - decimal_places)
    } else {
        fraction * 10u128.pow(decimal_places - 6)
    };
    let mut fraction = format!("{fraction:0width$}", width = decimal_places as usize);
    if rounded_whole == 0 {
        while fraction.len() > 3 && fraction.ends_with('0') {
            fraction.pop();
        }
        while fraction.len() < 3 {
            fraction.push('0');
        }
    }
    format!("${rounded_whole}.{fraction}")
}

fn push_narrower_variant(variants: &mut Vec<String>, candidate: String) {
    if candidate.is_empty() || variants.iter().any(|variant| variant == &candidate) {
        return;
    }
    if variants
        .last()
        .is_none_or(|previous| visible_width(&candidate) < visible_width(previous))
    {
        variants.push(candidate);
    }
}

fn identity_variants(full_model: &str, model_names: &[String], thinking: &str) -> Vec<String> {
    let mut variants = Vec::new();
    if !thinking.is_empty() && !thinking.eq_ignore_ascii_case("off") {
        push_narrower_variant(&mut variants, format!("{full_model} · {thinking}"));
    }
    for model in model_names {
        push_narrower_variant(&mut variants, model.clone());
    }
    if variants.is_empty() {
        variants.push(full_model.to_owned());
    }
    variants
}

fn activity_variants(_state: &super::view::ShellState, _now: Instant) -> Vec<String> {
    // Live activity belongs in the transcript: reasoning and tools share one
    // restrained margin pulse. Keeping the footer informational avoids a
    // second competing activity surface.
    Vec::new()
}

/// Render exactly one semantic, width-aware status row. The composer owns this
/// row; shell chrome must not reserve or append a second footer.
fn render_status_footer(state: &super::view::ShellState, width: u16, now: Instant) -> String {
    let layout = state.theme.layout_for_width(width);
    let total_width = usize::from(width);
    if total_width == 0 {
        return String::new();
    }
    let requested_inset = 1usize.saturating_add(usize::from(layout.composer_padding));
    let left_inset = if width >= 5 {
        requested_inset.min(total_width.saturating_sub(1) / 2)
    } else {
        0
    };
    let right_inset = left_inset;
    let available = total_width.saturating_sub(left_inset + right_inset);
    let gap = if width < 42 { 2 } else { 3 };
    let active = state.run.current().is_some_and(|run| run.is_active());
    let show_turn_telemetry = active || state.selected_model_owns_telemetry();

    // Active runs retain the identity and pricing captured at submission. Idle
    // rows immediately reflect the selected model. This prevents a queued model
    // switch from relabelling in-flight telemetry.
    let full_model = if active {
        state
            .run_model_display
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| {
                if state.model_display.trim().is_empty() {
                    state.model.as_str()
                } else {
                    state.model_display.as_str()
                }
            })
            .to_owned()
    } else if !state.model_display.trim().is_empty() {
        state.model_display.clone()
    } else if !state.model.trim().is_empty() {
        state.model.clone()
    } else {
        state.theme.glyph("wordmark").to_owned()
    };
    let mut model_names = if active {
        if state.run_model_compact_names.is_empty() {
            vec![full_model.clone()]
        } else {
            state.run_model_compact_names.clone()
        }
    } else if state.model_compact_names.is_empty() {
        vec![full_model.clone()]
    } else {
        state.model_compact_names.clone()
    };
    if model_names.first() != Some(&full_model) {
        model_names.insert(0, full_model.clone());
    }
    let thinking = if active {
        state.run_reasoning.as_deref().unwrap_or(&state.reasoning)
    } else {
        &state.reasoning
    }
    .trim();
    let mut segments = vec![FooterSegment::new(
        FooterKind::Identity,
        identity_variants(&full_model, &model_names, thinking),
    )];

    let base_context = if active {
        state.run_context_estimate
    } else {
        state.context_estimate
    };
    let displayed_context = base_context.map(|(used, limit)| {
        (
            used.saturating_add(if active {
                state.live_generated_tokens().unwrap_or_default()
            } else {
                0
            }),
            limit,
        )
    });
    if let Some((used, limit)) = displayed_context {
        let estimated = active && state.turn_generation_started_at.is_some();
        let marker = if estimated { "~" } else { "" };
        segments.push(FooterSegment::new(
            FooterKind::Context,
            vec![format!(
                "{marker}{}/{}",
                compact_token_count(used),
                compact_token_limit(limit)
            )],
        ));
    }

    if let Some((output, estimated)) = show_turn_telemetry
        .then(|| state.displayed_output_tokens())
        .flatten()
    {
        let output = compact_token_count(output);
        let marker = if estimated { "~" } else { "" };
        let input = state.last_turn_usage.map(|usage| {
            usage
                .input_tokens
                .saturating_add(usage.cache_read_tokens)
                .saturating_add(usage.cache_write_tokens)
        });
        let variants = match (unicode(state), input) {
            (true, Some(input)) => vec![
                format!("↑{} {marker}↓{output}", compact_token_count(input)),
                format!("{}/{marker}{output}", compact_token_count(input)),
            ],
            (false, Some(input)) => vec![
                format!("in {} {marker}out {output}", compact_token_count(input)),
                format!("{}/{marker}{output}", compact_token_count(input)),
            ],
            (true, None) => vec![format!("{marker}↓{output}")],
            (false, None) => vec![format!("{marker}out {output}")],
        };
        segments.push(FooterSegment::new(FooterKind::Tokens, variants));
    }

    // Keep throughput in `/status`, where its completed-turn provenance is
    // explicit. A live wall-clock average becomes misleading whenever output
    // pauses and adds constantly changing noise to this pinned summary.

    let price_display = if active {
        state.run_price_display.unwrap_or(state.price_display)
    } else {
        state.price_display
    };
    // This is the durable session total, not the cost accumulated by the
    // current autonomous run. Session spend remains meaningful when the
    // selected model changes, so it must not depend on turn telemetry ownership.
    let cost = if let Some(cost) = state.displayed_session_cost_microdollars() {
        Some(format_microdollars(cost))
    } else {
        match price_display {
            crate::presentation::PriceDisplay::ExplicitZero => Some("$0".to_owned()),
            crate::presentation::PriceDisplay::Unknown
            | crate::presentation::PriceDisplay::Priced => None,
        }
    };
    if let Some(cost) = cost {
        segments.push(FooterSegment::new(FooterKind::Cost, vec![cost]));
    }

    if let Some(basis_points) = show_turn_telemetry
        .then_some(state.cache_hit_rate_basis_points)
        .flatten()
    {
        let percent = f64::from(basis_points) / 100.0;
        segments.push(FooterSegment::new(
            FooterKind::CacheHit,
            vec![format!("cache {percent:.1}%")],
        ));
    }

    let activity = activity_variants(state, now);
    if !activity.is_empty() {
        segments.push(FooterSegment::new(FooterKind::Activity, activity));
    }

    // The identity moves to the header when that surface is active. Footer
    // visibility controls its fallback placement; status-line visibility
    // controls telemetry as semantic groups.
    if !layout.show_footer || layout.show_header {
        hide_footer_kind(&mut segments, FooterKind::Identity);
    }
    if !layout.show_status_line {
        for kind in [
            FooterKind::Tokens,
            FooterKind::CacheHit,
            FooterKind::Context,
            FooterKind::Cost,
            FooterKind::Activity,
        ] {
            hide_footer_kind(&mut segments, kind);
        }
    }

    // Remove the thinking qualifier first, then drop complete semantic groups
    // from lowest to highest retention priority. Model identity and active
    // state are kept longest; numeric instruments are never byte-truncated.
    if footer_width(&segments, gap) > available {
        compact_footer_kind(&mut segments, FooterKind::Identity);
    }
    for kind in [FooterKind::CacheHit, FooterKind::Tokens] {
        if footer_width(&segments, gap) > available {
            hide_footer_kind(&mut segments, kind);
        }
    }
    while footer_width(&segments, gap) > available {
        let before = footer_width(&segments, gap);
        compact_footer_kind(&mut segments, FooterKind::Context);
        if footer_width(&segments, gap) == before {
            break;
        }
    }
    if footer_width(&segments, gap) > available {
        hide_footer_kind(&mut segments, FooterKind::Context);
    }
    while footer_width(&segments, gap) > available {
        let before = footer_width(&segments, gap);
        compact_footer_kind(&mut segments, FooterKind::Identity);
        if footer_width(&segments, gap) == before {
            break;
        }
    }
    if footer_width(&segments, gap) > available {
        hide_footer_kind(&mut segments, FooterKind::Cost);
    }
    if footer_width(&segments, gap) > available {
        // An active state always remains observable. At extremely narrow
        // widths it is more useful than an un-attributed fragment of a model
        // name; idle rows have no activity segment and keep identity instead.
        if active {
            hide_footer_kind(&mut segments, FooterKind::Identity);
        } else {
            hide_footer_kind(&mut segments, FooterKind::Activity);
        }
    }

    let context_is_urgent = displayed_context
        .is_some_and(|(used, limit)| limit > 0 && used as f64 * 100.0 / limit as f64 >= 90.0);
    let style_segment = |segment: &FooterSegment| match segment.kind {
        FooterKind::Identity => state.theme.model_fg(
            if active {
                state.run_model_lab
            } else {
                state.model_lab
            },
            segment.text(),
        ),
        FooterKind::Context if context_is_urgent => state.theme.fg("error", segment.text()),
        FooterKind::Cost
            if state
                .session_cost_microdollars
                .zip(state.max_session_cost_microdollars)
                .is_some_and(|(cost, limit)| limit > 0 && cost >= limit.saturating_mul(9) / 10) =>
        {
            state.theme.fg("error", segment.text())
        }
        FooterKind::Cost
            if state
                .session_cost_microdollars
                .zip(state.max_session_cost_microdollars)
                .is_some_and(|(cost, limit)| limit > 0 && cost >= limit / 2) =>
        {
            state.theme.fg("warning", segment.text())
        }
        FooterKind::Activity => state.theme.fg("foreground", segment.text()),
        _ => state.theme.fg("muted", segment.text()),
    };

    // Activity is a pinned right-hand instrument rather than another item in
    // the left telemetry sentence. Its semantic width still participates in
    // the collapse policy above, then any spare cells become stable whitespace
    // between the two zones. This keeps the state/stopwatch visually fixed as
    // token counts change.
    let left = segments
        .iter()
        .filter(|segment| segment.visible && segment.kind != FooterKind::Activity)
        .collect::<Vec<_>>();
    let activity = segments
        .iter()
        .find(|segment| segment.visible && segment.kind == FooterKind::Activity);
    let left_width = left
        .iter()
        .map(|segment| visible_width(segment.text()))
        .sum::<usize>()
        + left.len().saturating_sub(1) * gap;
    let left_styled = left
        .iter()
        .map(|segment| style_segment(segment))
        .collect::<Vec<_>>()
        .join(&" ".repeat(gap));

    let body = if let Some(activity) = activity {
        let activity_width = visible_width(activity.text());
        let spacing = if left.is_empty() {
            available.saturating_sub(activity_width)
        } else {
            available.saturating_sub(left_width + activity_width)
        };
        format!(
            "{left_styled}{}{activity}",
            " ".repeat(spacing),
            activity = style_segment(activity)
        )
    } else {
        left_styled
    };
    let line = format!("{}{body}", " ".repeat(left_inset));
    fit_line(&line, width)
}

pub(crate) fn status_footer_visible(state: &super::view::ShellState, width: u16) -> bool {
    let layout = state.theme.layout_for_width(width);
    let has_identity = layout.show_footer && !layout.show_header;
    has_identity || layout.show_status_line
}

fn append_status_footer(
    lines: &mut Vec<String>,
    state: &super::view::ShellState,
    width: u16,
    now: Instant,
) {
    if status_footer_visible(state, width) {
        lines.push(render_status_footer(state, width, now));
    }
}

/// Format a token count compactly: `1.2k`, `856`, `1.0m`.
pub(crate) fn compact_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn compact_token_limit(n: u64) -> String {
    if n >= 1_000_000 && n % 1_000_000 == 0 {
        format!("{}m", n / 1_000_000)
    } else if n >= 1_000 && n % 1_000 == 0 {
        format!("{}k", n / 1_000)
    } else {
        compact_token_count(n)
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Render the full composer surface: a top rule, content rows, a bottom rule,
/// then a status footer line.
pub fn render_composer_surface(
    state: &super::view::ShellState,
    width: u16,
    now: Instant,
) -> Vec<String> {
    let w = usize::from(width);
    if w < 3 {
        let prompt = if state.editor.is_empty() {
            fit_line(state.theme.glyph("prompt"), width)
        } else {
            let (editor, _) = super::view::sanitized_editor(&state.editor, state.editor_cursor);
            fit_line(&format!("> {editor}"), width)
        };
        let mut lines = vec![prompt];
        append_status_footer(&mut lines, state, width, now);
        return lines;
    }

    let term_rows = state.size.1;
    // Use wrapped (visual) line count so a single long line that wraps
    // across several rows is counted properly when deciding how tall the
    // composer box should be.
    let (editor, editor_cursor) = super::view::sanitized_editor(&state.editor, state.editor_cursor);
    let layout = state.theme.layout_for_width(width);
    let editor_width = if w < 12 {
        let padding = layout.composer_padding.min(width.saturating_sub(3));
        width.saturating_sub(padding.saturating_add(2)).max(1)
    } else {
        // The boxed composer frames with top and bottom rules only, so its
        // content spans the full terminal width.
        width.max(1)
    };
    let visual_lines = if editor.is_empty() {
        1
    } else {
        state
            .cached_editor_layout(editor_width.max(2), Some(&editor), Some(editor_cursor))
            .lines
            .len()
            .max(1)
    };
    let content_rows = composer_content_rows(term_rows, visual_lines);

    if w < 12 {
        return render_compact(state, width, now, content_rows);
    }

    let mut lines = Vec::with_capacity(content_rows + 4);

    // Unified composer frame with stable model-adaptive rules.
    lines.append(&mut render_composer_box(state, width, now, content_rows));

    // Stable semantic footer/status surface.
    append_status_footer(&mut lines, state, width, now);

    lines
}

/// Narrow-terminal fallback: no box, just model line + prompt.
fn render_compact(
    state: &super::view::ShellState,
    width: u16,
    now: Instant,
    content_rows: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    let w = usize::from(width);
    let padding_width =
        usize::from(state.theme.layout_for_width(width).composer_padding).min(w.saturating_sub(3));
    let padding = " ".repeat(padding_width);

    // Prompt. The single status row is appended below it, matching the boxed
    // composer geometry used at ordinary widths.
    let marker = state.theme.glyph("prompt");
    let marker_s = state.theme.bold(&state.theme.model_fg(
        if state.run.is_active() {
            state.run_model_lab
        } else {
            state.model_lab
        },
        marker,
    ));
    let cursor_marker = composer_cursor_marker(state);
    let (editor, editor_cursor) = super::view::sanitized_editor(&state.editor, state.editor_cursor);

    if editor.is_empty() {
        lines.push(fit_line(
            &format!("{padding}{marker_s} {cursor_marker}"),
            width,
        ));
        append_status_footer(&mut lines, state, width, now);
        return lines;
    }

    let inner_w = w.saturating_sub(padding_width + 2).max(1);
    let layout = state.cached_editor_layout(inner_w as u16, Some(&editor), Some(editor_cursor));
    let visible_rows = content_rows.max(1).min(layout.lines.len());
    let mut start = layout
        .cursor_row
        .saturating_add(1)
        .saturating_sub(visible_rows);
    let end = (start + visible_rows).min(layout.lines.len());
    if end.saturating_sub(start) < visible_rows {
        start = end.saturating_sub(visible_rows);
    }

    for index in start..end {
        let vis_line = &layout.lines[index];
        let content = if index == layout.cursor_row {
            let cursor = editor_cursor.clamp(vis_line.start, vis_line.visible_end);
            format!(
                "{}{cursor_marker}{}",
                &editor[vis_line.start..cursor],
                &editor[cursor..vis_line.visible_end]
            )
        } else {
            editor[vis_line.start..vis_line.visible_end].to_owned()
        };
        let prefix = if index == 0 {
            format!("{padding}{marker_s} ")
        } else {
            format!("{padding}  ")
        };
        lines.push(fit_line(&format!("{prefix}{content}"), width));
    }
    append_status_footer(&mut lines, state, width, now);
    lines
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_rows_starts_at_one() {
        // 40-row terminal → max 8 rows (40/5 clamped to 4..14)
        assert_eq!(composer_content_rows(40, 1), 1);
        assert_eq!(composer_content_rows(40, 4), 4);
        assert_eq!(composer_content_rows(40, 20), 8); // capped at 8
                                                      // 12-row terminal → max 3 rows
        assert_eq!(composer_content_rows(12, 1), 1);
        assert_eq!(composer_content_rows(12, 10), 3);
        // 20-row terminal → max 5 rows
        assert_eq!(composer_content_rows(20, 7), 5); // capped at 5
    }

    #[test]
    fn footer_cost_rounds_to_three_significant_figures() {
        for (microdollars, expected) in [
            (0, "$0.000"),
            (123_456, "$0.123"),
            (12_345, "$0.0123"),
            (1, "$0.000001"),
            (1_234_567, "$1.23"),
            (12_345_678, "$12.3"),
            (123_456_789, "$123"),
            (9_999_999, "$10.0"),
        ] {
            assert_eq!(format_microdollars(microdollars), expected);
        }
    }

    #[test]
    fn overflow_count() {
        assert_eq!(composer_overflow_count(5, 3), 2);
        assert_eq!(composer_overflow_count(3, 5), 0);
    }
}
