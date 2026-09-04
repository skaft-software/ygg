//! Composer surface: an inset multiline input area framed by stable
//! model-adaptive rules, with a calm semantic status footer below.

use std::time::Instant;

use sexy_tui_rs::{visible_width, TextEditAction, TextEditor, TextEditorProjection, CURSOR_MARKER};

use crate::presentation::compact_context_limit;
use crate::tui::layout::PresentationLayout;
use crate::tui::view::{fit_line, footer_width, EditorDisplayMap, FooterSegment};

fn composer_cursor_marker(state: &super::view::ShellState) -> &'static str {
    if state.panel.is_some() {
        ""
    } else {
        CURSOR_MARKER
    }
}

/// Cache key for one Ygg-owned composer source. The generic editor's revision
/// lets the shell reuse a transformed layout without hashing a full draft on
/// every frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ComposerEditorSource {
    Draft(u64),
    ToolPrompt(u64),
}

/// One transformed display layout for the current source and text-cell width.
pub(crate) struct ComposerEditorCache {
    source: ComposerEditorSource,
    text_width: usize,
    projection: ComposerEditorProjection,
}

impl ComposerEditorCache {
    #[must_use]
    pub(crate) fn new(
        source: ComposerEditorSource,
        text: &str,
        cursor: usize,
        text_width: usize,
    ) -> Self {
        Self {
            source,
            text_width,
            projection: ComposerEditorProjection::new(text, cursor, text_width),
        }
    }

    #[must_use]
    pub(crate) fn matches(&self, source: ComposerEditorSource, text_width: usize) -> bool {
        self.source == source && self.text_width == text_width
    }

    #[must_use]
    pub(crate) fn projection(&self) -> &ComposerEditorProjection {
        &self.projection
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn source(&self) -> ComposerEditorSource {
        self.source
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn text_width(&self) -> usize {
        self.text_width
    }
}

/// A safe, lazily materialized display projection for the composer.
///
/// Sanitization and source/display mapping remain Ygg policy. Visual wrapping,
/// cursor coordinates, and visual navigation come from the generic editor.
pub(crate) struct ComposerEditorProjection {
    display: EditorDisplayMap,
    projection: TextEditorProjection,
}

impl ComposerEditorProjection {
    fn new(source: &str, source_cursor: usize, text_width: usize) -> Self {
        let display = EditorDisplayMap::from_source(source);
        let display_cursor = display.source_to_display(source_cursor);
        let projection =
            TextEditor::projection_for(display.layout_text(), display_cursor, text_width);
        Self {
            display,
            projection,
        }
    }

    #[must_use]
    pub(crate) fn line_count(&self) -> usize {
        self.projection.lines().len()
    }

    #[must_use]
    pub(crate) fn cursor_row(&self) -> usize {
        self.projection.cursor_row()
    }

    #[must_use]
    pub(crate) fn visible_row(&self, row: usize, cursor_marker: &str) -> String {
        let Some(line) = self.projection.lines().get(row) else {
            return String::new();
        };
        let cursor = (row == self.projection.cursor_row() && !cursor_marker.is_empty())
            .then(|| self.projection.cursor().offset());
        self.display
            .terminal_row(line.start(), line.visible_end(), cursor, cursor_marker)
    }

    #[must_use]
    pub(crate) fn visual_source_target(
        &self,
        action: &TextEditAction,
        preferred_column: &mut Option<usize>,
    ) -> Option<usize> {
        self.projection
            .visual_target(self.display.layout_text(), action, preferred_column)
            .map(|display| self.display.display_to_source(display))
    }

    #[must_use]
    pub(crate) fn layout_text(&self) -> &str {
        self.display.layout_text()
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
    crate::tui::layout::composer_content_rows(terminal_rows, visual_lines)
}

// ---------------------------------------------------------------------------
// Bordered composer box
// ---------------------------------------------------------------------------

fn horiz(state: &super::view::ShellState) -> &str {
    state.theme.glyph("horizontal")
}

// ---------------------------------------------------------------------------
// Unified composer frame
// ---------------------------------------------------------------------------

/// Composer chrome selected by the theme's open `composer` token.
/// `boxed` (default): full-width top/bottom rules, no corners.
/// `framed`: a cornered box (Claude Code / pi style) coloured by
/// `composer_border` (token reference; falls back to the next model's accent).
/// `shaded`: no rules at all — a rectangle painted with `composer_bg`
/// (codex style). Unresolvable backgrounds degrade to plain rows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ComposerChrome {
    Boxed,
    Framed,
    Shaded,
}

fn composer_chrome(theme: &super::theme::YggTheme) -> ComposerChrome {
    match theme
        .resolve::<String>("composer")
        .as_deref()
        .map(str::trim)
    {
        Some("framed") => ComposerChrome::Framed,
        Some("shaded") => ComposerChrome::Shaded,
        _ => ComposerChrome::Boxed,
    }
}

/// Width available to composer content rows for the active chrome. Framed
/// chrome spends two border columns and one space of padding per side;
/// shaded chrome keeps one painted margin column per side.
fn composer_inner_width(theme: &super::theme::YggTheme, frame_width: u16) -> u16 {
    match composer_chrome(theme) {
        ComposerChrome::Boxed => frame_width,
        ComposerChrome::Framed => frame_width.saturating_sub(4),
        ComposerChrome::Shaded => frame_width.saturating_sub(2),
    }
    .max(1)
}

/// One chrome-aware geometry decision for editor movement, sizing, layout, and
/// rendering. The prompt glyph, its gap, and one spare hardware-cursor cell are
/// always reserved, even while a panel suppresses the marker, so focus changes
/// never reflow a draft.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ComposerEditorGeometry {
    inner_width: usize,
    text_width: usize,
}

impl ComposerEditorGeometry {
    #[must_use]
    pub(crate) fn inner_width(self) -> usize {
        self.inner_width
    }

    #[must_use]
    pub(crate) fn text_width(self) -> usize {
        self.text_width
    }
}

#[must_use]
pub(crate) fn composer_editor_geometry(
    state: &super::view::ShellState,
    width: u16,
) -> ComposerEditorGeometry {
    const PROMPT_GAP: usize = 1;
    const CURSOR_CELL: usize = 1;

    let presentation = PresentationLayout::new(&state.theme, width);
    let compact = usize::from(width) < 12;
    let inner_width = if compact {
        usize::from(presentation.content_width)
    } else {
        usize::from(composer_inner_width(
            &state.theme,
            presentation.content_width,
        ))
    };
    let prompt_and_gap = visible_width(state.theme.glyph("prompt")).saturating_add(PROMPT_GAP);
    let text_width = inner_width
        .saturating_sub(prompt_and_gap.saturating_add(CURSOR_CELL))
        .max(1);
    ComposerEditorGeometry {
        inner_width,
        text_width,
    }
}

/// Render composer content inside the shared horizontal inset. Plain boxed
/// rules span the terminal, while explicit framed/shaded theme chrome stays
/// on the content grid. Selected prompt text copies without chrome. Rules
/// remain byte-stable while a draft changes or work runs; live motion belongs
/// to the semantic transcript entry doing the work.
fn render_composer_box(
    state: &super::view::ShellState,
    width: u16,
    _now: Instant,
    content_rows: usize,
    geometry: ComposerEditorGeometry,
    editor: &ComposerEditorProjection,
) -> Vec<String> {
    let terminal_width = usize::from(width);
    if terminal_width < 4 {
        return render_plain_content(state, width, editor);
    }

    let theme = &state.theme;
    let layout = PresentationLayout::new(theme, width);
    let inset = usize::from(layout.inset);
    let frame_width = usize::from(layout.content_width);
    let frame_prefix = " ".repeat(inset);
    let h = horiz(state);
    let chrome = composer_chrome(theme);
    let inner_width = geometry.inner_width();

    // The composer always identifies the model selected for the next prompt.
    // It never follows a prior prompt or a generic application accent, and it
    // does not change colour merely because work starts or the draft changes.
    let model_accent = theme.model_rgb(state.model_lab).unwrap_or((128, 128, 128));
    let border_rgb = theme.role_rgb("composer_border").unwrap_or(model_accent);
    let shaded_bg = (chrome == ComposerChrome::Shaded)
        .then(|| theme.role_rgb("composer_bg"))
        .flatten();
    let render_rule = || -> String { theme.rgb_fg(border_rgb, &h.repeat(terminal_width)) };
    let frame_rule = |left: &str, right: &str| -> String {
        format!(
            "{frame_prefix}{}",
            theme.rgb_fg(
                border_rgb,
                &format!("{left}{}{right}", h.repeat(frame_width.saturating_sub(2))),
            )
        )
    };
    let finish_row = |row: String| -> String {
        let row = match chrome {
            ComposerChrome::Boxed => row,
            ComposerChrome::Framed => {
                let vertical = theme.rgb_fg(border_rgb, theme.glyph("vertical"));
                format!("{vertical} {row} {vertical}")
            }
            ComposerChrome::Shaded => {
                let padded = format!(" {row} ");
                match shaded_bg {
                    Some(bg) => theme.paint_row_background(bg, &padded),
                    None => padded,
                }
            }
        };
        fit_line(&format!("{frame_prefix}{row}"), width)
    };
    let blank_shaded_row = || -> String {
        let padded = " ".repeat(frame_width);
        let row = match shaded_bg {
            Some(bg) => theme.paint_row_background(bg, &padded),
            None => padded,
        };
        format!("{frame_prefix}{row}")
    };

    let mut lines = Vec::with_capacity(content_rows + 2);
    match chrome {
        ComposerChrome::Boxed => lines.push(render_rule()),
        ComposerChrome::Framed => lines.push(frame_rule(
            theme.glyph("top_left"),
            theme.glyph("top_right"),
        )),
        ComposerChrome::Shaded => lines.push(blank_shaded_row()),
    }

    let marker = theme.bold(&theme.model_fg(state.model_lab, theme.glyph("prompt")));
    let cursor_marker = composer_cursor_marker(state);
    let render_row = |content: &str| -> String {
        let content_width = visible_width(content);
        if content_width > inner_width {
            fit_line(content, inner_width as u16)
        } else {
            format!(
                "{content}{}",
                " ".repeat(inner_width.saturating_sub(content_width))
            )
        }
    };

    if editor.layout_text().is_empty() {
        for index in 0..content_rows {
            if index == 0 {
                lines.push(finish_row(render_row(&format!("{marker} {cursor_marker}"))));
            } else {
                lines.push(finish_row(render_row("")));
            }
        }
    } else {
        let total_lines = editor.line_count();
        let overflow = total_lines.saturating_sub(content_rows);
        let visible_rows = if overflow > 0 {
            (content_rows.saturating_sub(1)).max(1).min(total_lines)
        } else {
            content_rows.max(1).min(total_lines)
        };
        let mut start = editor
            .cursor_row()
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
            let message = format!(
                "{ellipsis} {hidden_above} more line{} above",
                if hidden_above == 1 { "" } else { "s" }
            );
            rendered.push(render_row(&theme.fg("accent", &message)));
        }

        for index in start..end {
            let prefix = if index == 0 {
                format!("{marker} ")
            } else {
                "  ".to_owned()
            };
            let row = editor.visible_row(index, cursor_marker);
            rendered.push(render_row(&format!("{prefix}{row}")));
        }

        if hidden_below > 0 {
            let ellipsis = theme.glyph("ellipsis");
            let message = format!(
                "{ellipsis} {hidden_below} more line{} below",
                if hidden_below == 1 { "" } else { "s" }
            );
            rendered.push(render_row(&theme.fg("accent", &message)));
        }

        while rendered.len() < content_rows {
            rendered.push(render_row(""));
        }
        lines.extend(rendered.into_iter().map(&finish_row));
    }

    match chrome {
        ComposerChrome::Boxed => lines.push(render_rule()),
        ComposerChrome::Framed => lines.push(frame_rule(
            theme.glyph("bottom_left"),
            theme.glyph("bottom_right"),
        )),
        ComposerChrome::Shaded => lines.push(blank_shaded_row()),
    }

    lines
}

// ---------------------------------------------------------------------------
// Plain content fallback (very narrow terminals)
// ---------------------------------------------------------------------------

fn render_plain_content(
    state: &super::view::ShellState,
    width: u16,
    editor: &ComposerEditorProjection,
) -> Vec<String> {
    let marker = state.theme.bold(
        &state
            .theme
            .model_fg(state.model_lab, state.theme.glyph("prompt")),
    );
    let cursor_marker = composer_cursor_marker(state);
    if editor.layout_text().is_empty() {
        return vec![fit_line(&format!("{cursor_marker}{marker}"), width)];
    }
    let row = editor.visible_row(editor.cursor_row(), cursor_marker);
    vec![fit_line(&format!("{marker} {row}"), width)]
}

// ---------------------------------------------------------------------------
// Status footer (below the composer box)
// ---------------------------------------------------------------------------

/// Semantic footer group. Variants are ordered from most descriptive to
/// most compact; groups disappear as units rather than being byte-truncated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterKind {
    Identity,
    Context,
    Cost,
}

/// The composer retains its domain role beside the shared, production footer
/// segment primitive. Ordinary action footers and status chrome therefore use
/// the same width and complete-segment vocabulary without coupling their data.
struct StatusFooterSegment {
    kind: FooterKind,
    segment: FooterSegment,
}

impl StatusFooterSegment {
    fn new(kind: FooterKind, variants: Vec<String>) -> Self {
        Self {
            kind,
            segment: FooterSegment::variants(variants),
        }
    }
}

fn status_footer_width(segments: &[StatusFooterSegment], gap: usize) -> usize {
    let segments = segments
        .iter()
        .filter(|segment| segment.segment.is_visible())
        .map(|segment| segment.segment.clone())
        .collect::<Vec<_>>();
    footer_width(&segments, gap)
}

fn hide_footer_kind(segments: &mut [StatusFooterSegment], kind: FooterKind) {
    if let Some(segment) = segments.iter_mut().find(|segment| segment.kind == kind) {
        segment.segment.hide();
    }
}

fn compact_footer_kind(segments: &mut [StatusFooterSegment], kind: FooterKind) {
    if let Some(segment) = segments.iter_mut().find(|segment| segment.kind == kind) {
        segment.segment.compact_once();
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

fn identity_variants(
    full_model: &str,
    model_names: &[String],
    thinking: &str,
    separator: &str,
) -> Vec<String> {
    let mut variants = Vec::new();
    if !thinking.is_empty() && !thinking.eq_ignore_ascii_case("off") {
        push_narrower_variant(&mut variants, format!("{full_model}{separator}{thinking}"));
    }
    for model in model_names {
        push_narrower_variant(&mut variants, model.clone());
    }
    if variants.is_empty() {
        variants.push(full_model.to_owned());
    }
    variants
}

fn context_percent(used: u64, limit: u64) -> u64 {
    if limit == 0 {
        return 0;
    }
    let rounded = (u128::from(used) * 100 + u128::from(limit) / 2) / u128::from(limit);
    u64::try_from(rounded).unwrap_or(u64::MAX)
}

/// Render one calm, width-aware status row. Detailed cache and input/output
/// accounting stays in `/status`, `/cost`, and `/cache`; default chrome keeps
/// only identity, context pressure, and durable session spend.
fn render_status_footer(state: &super::view::ShellState, width: u16, _now: Instant) -> String {
    let theme_layout = state.theme.layout_for_width(width);
    let presentation = PresentationLayout::new(&state.theme, width);
    let total_width = usize::from(width);
    if total_width == 0 {
        return String::new();
    }
    // The footer is metadata, so it begins on the shared primary-text column
    // rather than on the full-width surface edge used by rules and cards.
    // Keep this inset independent from the composer frame itself.
    let requested_inset = 1usize.saturating_add(usize::from(theme_layout.composer_padding));
    let left_inset = if width >= 5 {
        requested_inset.min(total_width.saturating_sub(1) / 2)
    } else {
        0
    };
    let available = total_width.saturating_sub(left_inset.saturating_mul(2));
    let gap = presentation.footer_gap;
    let active = state.run.current().is_some_and(|run| run.is_active());

    // Active runs retain the identity captured at submission. An idle footer
    // immediately reflects the selected model without recolouring the chrome.
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
    let effort = if active {
        state.run_reasoning.as_deref().unwrap_or(&state.reasoning)
    } else {
        &state.reasoning
    }
    .trim();
    let mut segments = vec![StatusFooterSegment::new(
        FooterKind::Identity,
        identity_variants(
            &full_model,
            &model_names,
            effort,
            super::view::semantic_separator(&state.theme),
        ),
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
    if let Some((used, limit)) = displayed_context.filter(|(_, limit)| *limit > 0) {
        let estimated = active && state.turn_generation_started_at.is_some();
        let marker = if estimated { "~" } else { "" };
        let percent = context_percent(used, limit);
        let limit_display = compact_context_limit(limit);
        segments.push(StatusFooterSegment::new(
            FooterKind::Context,
            vec![
                format!("context {marker}{percent}%/{limit_display}"),
                format!("{marker}{percent}%/{limit_display}"),
            ],
        ));
    }

    let price_display = if active {
        state.run_price_display.unwrap_or(state.price_display)
    } else {
        state.price_display
    };
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
        segments.push(StatusFooterSegment::new(
            FooterKind::Cost,
            vec![format!("session {cost}")],
        ));
    }

    if !theme_layout.show_footer || theme_layout.show_header {
        hide_footer_kind(&mut segments, FooterKind::Identity);
    }
    if !theme_layout.show_status_line {
        hide_footer_kind(&mut segments, FooterKind::Context);
        hide_footer_kind(&mut segments, FooterKind::Cost);
    }

    if status_footer_width(&segments, gap) > available {
        compact_footer_kind(&mut segments, FooterKind::Identity);
    }
    if status_footer_width(&segments, gap) > available {
        compact_footer_kind(&mut segments, FooterKind::Context);
    }
    if status_footer_width(&segments, gap) > available {
        hide_footer_kind(&mut segments, FooterKind::Cost);
    }
    while status_footer_width(&segments, gap) > available {
        let before = status_footer_width(&segments, gap);
        compact_footer_kind(&mut segments, FooterKind::Identity);
        if status_footer_width(&segments, gap) == before {
            break;
        }
    }
    if status_footer_width(&segments, gap) > available {
        hide_footer_kind(&mut segments, FooterKind::Context);
    }
    if status_footer_width(&segments, gap) > available {
        hide_footer_kind(&mut segments, FooterKind::Identity);
    }

    let context_is_urgent = displayed_context
        .is_some_and(|(used, limit)| limit > 0 && u128::from(used) * 100 >= u128::from(limit) * 90);
    let style_segment = |segment: &StatusFooterSegment| match segment.kind {
        FooterKind::Identity => state.theme.fg("foreground", segment.segment.text()),
        FooterKind::Context if context_is_urgent => state.theme.fg("error", segment.segment.text()),
        FooterKind::Cost
            if state
                .session_cost_microdollars
                .zip(state.max_session_cost_microdollars)
                .is_some_and(|(cost, limit)| limit > 0 && cost >= limit.saturating_mul(9) / 10) =>
        {
            state.theme.fg("error", segment.segment.text())
        }
        FooterKind::Cost
            if state
                .session_cost_microdollars
                .zip(state.max_session_cost_microdollars)
                .is_some_and(|(cost, limit)| limit > 0 && cost >= limit / 2) =>
        {
            state.theme.fg("warning", segment.segment.text())
        }
        _ => state.theme.fg("muted", segment.segment.text()),
    };

    let identity = segments
        .iter()
        .find(|segment| segment.segment.is_visible() && segment.kind == FooterKind::Identity);
    let metrics = segments
        .iter()
        .filter(|segment| segment.segment.is_visible() && segment.kind != FooterKind::Identity)
        .collect::<Vec<_>>();
    let identity_text = identity.map(style_segment).unwrap_or_default();
    let identity_width = identity.map_or(0, |segment| visible_width(segment.segment.text()));
    let metrics_width = metrics
        .iter()
        .map(|segment| visible_width(segment.segment.text()))
        .sum::<usize>()
        + metrics.len().saturating_sub(1) * gap;
    let metrics_text = metrics
        .iter()
        .map(|segment| style_segment(segment))
        .collect::<Vec<_>>()
        .join(&" ".repeat(gap));
    let body = if identity.is_some() && !metrics.is_empty() {
        let spacing = available
            .saturating_sub(identity_width + metrics_width)
            .max(gap);
        format!("{identity_text}{}{metrics_text}", " ".repeat(spacing))
    } else {
        format!("{identity_text}{metrics_text}")
    };
    fit_line(&format!("{}{body}", " ".repeat(left_inset)), width)
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
    let geometry = composer_editor_geometry(state, width);
    // The same cache and text-cell width drive height calculation, visual
    // movement, and paint. Rendering then borrows only the viewport rows.
    let visual_lines = state
        .composer_editor_projection(geometry)
        .line_count()
        .max(1);
    let content_rows = composer_content_rows(state.size.1, visual_lines);

    if w < 3 {
        // There is no room for both prompt chrome and draft text. Put the
        // trusted cursor token first so the terminal backend can still place
        // its hardware cursor on a non-empty focused draft at widths 1 and 2.
        let cursor_marker = composer_cursor_marker(state);
        let mut lines = if cursor_marker.is_empty() {
            let editor = state.composer_editor_projection(geometry);
            render_plain_content(state, width, &editor)
        } else {
            let prompt = state.theme.bold(
                &state
                    .theme
                    .model_fg(state.model_lab, state.theme.glyph("prompt")),
            );
            vec![fit_line(&format!("{cursor_marker}{prompt}"), width)]
        };
        append_status_footer(&mut lines, state, width, now);
        return lines;
    }

    if w < 12 {
        return render_compact(state, width, now, content_rows, geometry);
    }

    let mut lines = Vec::with_capacity(content_rows + 4);

    // Unified inset frame with stable Ygg-owned focus rules.
    let editor = state.composer_editor_projection(geometry);
    lines.append(&mut render_composer_box(
        state,
        width,
        now,
        content_rows,
        geometry,
        &editor,
    ));

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
    geometry: ComposerEditorGeometry,
) -> Vec<String> {
    let mut lines = Vec::new();
    let plan = PresentationLayout::new(&state.theme, width);
    let padding_width = usize::from(plan.inset);
    let padding = " ".repeat(padding_width);

    // Prompt. The single status row is appended below it, matching the boxed
    // composer geometry used at ordinary widths.
    let marker = state.theme.glyph("prompt");
    let marker_s = state
        .theme
        .bold(&state.theme.model_fg(state.model_lab, marker));
    let cursor_marker = composer_cursor_marker(state);
    let editor = state.composer_editor_projection(geometry);

    if editor.layout_text().is_empty() {
        lines.push(fit_line(
            &format!("{padding}{marker_s} {cursor_marker}"),
            width,
        ));
        append_status_footer(&mut lines, state, width, now);
        return lines;
    }

    let total_lines = editor.line_count();
    let visible_rows = content_rows.max(1).min(total_lines);
    let mut start = editor
        .cursor_row()
        .saturating_add(1)
        .saturating_sub(visible_rows);
    let end = (start + visible_rows).min(total_lines);
    if end.saturating_sub(start) < visible_rows {
        start = end.saturating_sub(visible_rows);
    }

    for index in start..end {
        let prefix = if index == 0 {
            format!("{padding}{marker_s} ")
        } else {
            format!("{padding}  ")
        };
        let row = editor.visible_row(index, cursor_marker);
        lines.push(fit_line(&format!("{prefix}{row}"), width));
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
}
