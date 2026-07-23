//! Composer surface: a clean bordered multiline input area with a stable
//! status footer below, model-adaptive colour, truecolour gradient on the
//! top edge, and the Ygg flowing-gradient working animation.

use std::time::{Duration, Instant};

use sexy_tui_rs::{visible_width, CURSOR_MARKER};

use crate::presentation::format_duration;
use crate::tui::terminal::ColorDepth;
use crate::tui::view::fit_line;

/// How many full gradient cycles per second. The wave uses one fixed velocity
/// in every working phase, so a provider/tool transition cannot make it jump
/// faster or slower.
const ANIMATION_FREQ: f64 = 1.0;
const ANIMATION_SPEED: f64 = 1.0;

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
// Colour helpers
// ---------------------------------------------------------------------------

#[allow(dead_code)]
fn lighten(r: u8, g: u8, b: u8, factor: f64) -> (u8, u8, u8) {
    (
        (r as f64 + (255.0 - r as f64) * factor).round() as u8,
        (g as f64 + (255.0 - g as f64) * factor).round() as u8,
        (b as f64 + (255.0 - b as f64) * factor).round() as u8,
    )
}

#[allow(dead_code)]
fn darken(r: u8, g: u8, b: u8, factor: f64) -> (u8, u8, u8) {
    (
        (r as f64 * (1.0 - factor)).round() as u8,
        (g as f64 * (1.0 - factor)).round() as u8,
        (b as f64 * (1.0 - factor)).round() as u8,
    )
}

#[allow(dead_code)]
fn build_gradient(r: u8, g: u8, b: u8, width: usize) -> Vec<(u8, u8, u8)> {
    if width == 0 {
        return Vec::new();
    }
    let mut gradient = Vec::with_capacity(width);
    for i in 0..width {
        let pos = (i as f64) / (width.max(1) as f64);
        let factor = (1.0 - 2.0 * (pos - 0.5).abs()) * 0.35;
        let (cr, cg, cb) = if factor >= 0.0 {
            lighten(r, g, b, factor)
        } else {
            darken(r, g, b, -factor)
        };
        gradient.push((cr, cg, cb));
    }
    gradient
}

fn thinking_intensity(reasoning: &str, compacting: bool) -> f64 {
    if compacting {
        return 0.7;
    }
    match reasoning.trim().to_ascii_lowercase().as_str() {
        "off" => 0.0,
        "minimal" | "min" => 0.2,
        "low" => 0.35,
        "medium" | "med" => 0.5,
        "high" => 0.7,
        "xhigh" | "x-high" => 0.85,
        "max" => 1.0,
        _ => 0.7,
    }
}

// ---------------------------------------------------------------------------
// Animation — flowing gradient around the composer perimeter
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BorderPos {
    TopLeft,
    TopEdge { col: usize },
    TopRight,
    RightEdge { row: usize },
    BottomRight,
    BottomEdge { col: usize },
    BottomLeft,
    LeftEdge { row: usize },
}

#[derive(Clone, Copy, Debug)]
struct BorderCellStyle {
    rgb: (u8, u8, u8),
    encoding_key: u32,
    bold: bool,
}

impl BorderCellStyle {
    fn has_same_encoding(self, other: Self) -> bool {
        self.encoding_key == other.encoding_key && self.bold == other.bold
    }
}

fn get_gp(pos: BorderPos, w: usize, content_rows: usize) -> usize {
    match pos {
        BorderPos::TopLeft => 0,
        BorderPos::TopEdge { col } => col,
        BorderPos::TopRight => w.saturating_sub(1),
        BorderPos::RightEdge { row } => w + row,
        BorderPos::BottomRight => w + content_rows,
        BorderPos::BottomEdge { col } => {
            w + content_rows + (w.saturating_sub(1).saturating_sub(col))
        }
        BorderPos::BottomLeft => 2 * w + content_rows - 1,
        BorderPos::LeftEdge { row } => {
            2 * w + content_rows + (content_rows.saturating_sub(1).saturating_sub(row))
        }
    }
}

fn phase_speed(phase: Option<&crate::presentation::RunPhase>) -> f64 {
    phase_speed_for(phase)
}

/// Public version so the render loop can decide whether fast-refresh is needed.
/// Every working phase deliberately returns the same velocity. Waiting for
/// approval, a finished run, and idle state keep the border still.
pub fn phase_speed_for(phase: Option<&crate::presentation::RunPhase>) -> f64 {
    match phase {
        Some(crate::presentation::RunPhase::AwaitingApproval { .. })
        | Some(crate::presentation::RunPhase::Finished(_))
        | None => 0.0,
        Some(_) => ANIMATION_SPEED,
    }
}

// ---------------------------------------------------------------------------
// Bordered composer box
// ---------------------------------------------------------------------------

fn unicode(state: &super::view::ShellState) -> bool {
    state.theme.unicode()
}

fn glyphs(state: &super::view::ShellState) -> (&str, &str, &str, &str) {
    (
        state.theme.glyph("top_left"),
        state.theme.glyph("top_right"),
        state.theme.glyph("bottom_left"),
        state.theme.glyph("bottom_right"),
    )
}

fn horiz(state: &super::view::ShellState) -> &str {
    state.theme.glyph("horizontal")
}

fn vert(state: &super::view::ShellState) -> &str {
    state.theme.glyph("vertical")
}

// ---------------------------------------------------------------------------
// Unified composer box with perimeter shimmer
// ---------------------------------------------------------------------------

/// Render the entire bordered composer: top edge, content rows with side
/// borders, and bottom edge.  The shimmer travels clockwise around the full
/// perimeter.
fn render_composer_box(
    state: &super::view::ShellState,
    width: u16,
    now: Instant,
    content_rows: usize,
) -> Vec<String> {
    let w = usize::from(width);
    if w < 4 {
        return render_plain_content(state, width);
    }

    let theme = &state.theme;
    let horizontal_padding = usize::from(theme.layout_for_width(width).composer_padding);
    let inner_width = w.saturating_sub(2 + horizontal_padding.saturating_mul(2));
    if inner_width == 0 {
        return render_plain_content(state, width);
    }
    let padding = " ".repeat(horizontal_padding);
    let caps = theme.capabilities();
    let color_depth = caps.color;
    let (tl, tr, bl, br) = glyphs(state);
    let h = horiz(state);
    let v = vert(state);

    // ---- gradient / semantic border colours ----
    let run_active = state.run.is_active();
    let accent = if run_active {
        theme.model_rgb(state.run_model_lab)
    } else {
        theme.role_rgb("model_accent")
    }
    .unwrap_or((128, 128, 128));
    // Keep the resting border close to the terminal background. On a light
    // profile this moves toward white rather than turning into a black box;
    // focused input and active work use the model accent.
    let idle_border = theme.composer_idle_rgb(accent);
    let focused =
        state.panel.is_none() && (!state.editor.is_empty() || state.tool_input_prompt.is_some());

    // ---- animation ----
    let compacting = state.run_label == "compacting";
    let speed = if compacting {
        ANIMATION_SPEED
    } else {
        phase_speed(state.run.current().map(|r| r.phase()))
    };
    let animation_active = caps.animation
        && color_depth != ColorDepth::None
        && (run_active || compacting)
        && speed > 0.0;
    // Use one wall-clock anchor for the whole run instead of the current phase
    // elapsed time. Phase transitions can then update the footer without
    // resetting the wave or changing its apparent velocity.
    let elapsed = state
        .shimmer_started_at
        .map(|start| now.saturating_duration_since(start))
        .or_else(|| state.run.current().map(|run| run.phase_elapsed_at(now)))
        .unwrap_or(Duration::ZERO);
    let perimeter = 2 * w + 2 * content_rows;

    // Flowing-gradient phase offset: moves clockwise over time.
    let offset: f64 = if animation_active && perimeter > 0 {
        (elapsed.as_secs_f64() * ANIMATION_FREQ * speed).fract()
    } else {
        0.0
    };

    // Thinking level determines shimmer saturation/brightness.
    let reasoning = if run_active {
        state.run_reasoning.as_deref().unwrap_or(&state.reasoning)
    } else {
        &state.reasoning
    };
    let level_factor = thinking_intensity(reasoning, compacting);
    let shimmer_active = animation_active && level_factor > 0.0 && perimeter > 0;

    // Precompute the travelling wave for every perimeter position once per
    // frame.  Each border cell previously called cos() and powf() individually;
    // the LUT replaces ~250 float-CPU calls with a single indexed load.
    let wave_lut: Vec<f64> = if shimmer_active {
        (0..perimeter)
            .map(|gp| {
                let phase = ((gp as f64) / (perimeter as f64) - offset + 1.0).fract();
                let raw = ((phase - 0.5) * 2.0 * std::f64::consts::PI).cos();
                (raw * 0.5 + 0.5).powf(3.0)
            })
            .collect()
    } else {
        Vec::new()
    };

    // ---- colour helpers ----
    let border_style = |pos: BorderPos| -> BorderCellStyle {
        let (r, g, b) = accent;
        let (idle_r, idle_g, idle_b) = idle_border;

        let wave = if shimmer_active {
            wave_lut[get_gp(pos, w, content_rows)]
        } else {
            0.0
        };

        let (cr, cg, cb) = if shimmer_active {
            let curr_r =
                (idle_r as f64 + (r as f64 - idle_r as f64) * level_factor * wave).round() as u8;
            let curr_g =
                (idle_g as f64 + (g as f64 - idle_g as f64) * level_factor * wave).round() as u8;
            let curr_b =
                (idle_b as f64 + (b as f64 - idle_b as f64) * level_factor * wave).round() as u8;
            (curr_r, curr_g, curr_b)
        } else if focused || run_active || compacting {
            accent
        } else {
            (idle_r, idle_g, idle_b)
        };

        let rgb = (cr, cg, cb);
        BorderCellStyle {
            rgb,
            encoding_key: theme.rgb_fg_key(rgb),
            bold: color_depth == ColorDepth::Ansi16 && shimmer_active && wave > 0.5,
        }
    };

    let render_border_run = |style: BorderCellStyle, text: &str| -> String {
        let styled = theme.rgb_fg(style.rgb, text);
        if style.bold {
            theme.bold(&styled)
        } else {
            styled
        }
    };

    let border_cell =
        |pos: BorderPos, ch: &str| -> String { render_border_run(border_style(pos), ch) };

    // Horizontal edges dominate composer bytes. Group adjacent cells by the
    // colour sequence the terminal will actually receive (after ANSI
    // quantization), while retaining exact per-cell shimmer geometry.
    let render_horizontal_border = |top: bool| -> String {
        let mut line = String::with_capacity(w.saturating_mul(4));
        let mut run = String::new();
        let mut current_style: Option<BorderCellStyle> = None;
        for col in 0..w {
            let (pos, glyph) = if top {
                if col == 0 {
                    (BorderPos::TopLeft, tl)
                } else if col + 1 == w {
                    (BorderPos::TopRight, tr)
                } else {
                    (BorderPos::TopEdge { col }, h)
                }
            } else if col == 0 {
                (BorderPos::BottomLeft, bl)
            } else if col + 1 == w {
                (BorderPos::BottomRight, br)
            } else {
                (BorderPos::BottomEdge { col }, h)
            };
            let style = border_style(pos);
            if current_style.is_some_and(|current| current.has_same_encoding(style)) {
                run.push_str(glyph);
                continue;
            }
            if let Some(current) = current_style.replace(style) {
                line.push_str(&render_border_run(current, &run));
                run.clear();
            }
            run.push_str(glyph);
        }
        if let Some(current) = current_style {
            line.push_str(&render_border_run(current, &run));
        }
        line
    };

    let mut lines = Vec::with_capacity(content_rows + 2);

    // ---- top border ----
    lines.push(render_horizontal_border(true));

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
    let framed_row = |left: &str, content: &str, right: &str| {
        let content_width = visible_width(content);
        let content = if content_width > inner_width {
            fit_line(content, inner_width as u16)
        } else {
            format!(
                "{content}{}",
                " ".repeat(inner_width.saturating_sub(content_width))
            )
        };
        format!("{left}{padding}{content}{padding}{right}")
    };

    let (editor, editor_cursor) = if let Some(prompt) = &state.tool_input_prompt {
        (prompt.clone(), prompt.len())
    } else {
        super::view::sanitized_editor(&state.editor, state.editor_cursor)
    };

    if editor.is_empty() {
        for i in 0..content_rows {
            let left = border_cell(BorderPos::LeftEdge { row: i }, v);
            let right = border_cell(BorderPos::RightEdge { row: i }, v);
            if i == 0 {
                lines.push(framed_row(
                    &left,
                    &format!("{marker} {cursor_marker}"),
                    &right,
                ));
            } else {
                lines.push(framed_row(&left, "", &right));
            }
        }
    } else {
        let layout = state.cached_editor_layout(
            (inner_width as u16).max(2),
            Some(&editor),
            Some(editor_cursor),
        );
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
            let ri = rendered.len();
            let left = border_cell(BorderPos::LeftEdge { row: ri }, v);
            let right = border_cell(BorderPos::RightEdge { row: ri }, v);
            rendered.push(framed_row(&left, &theme.fg("model_accent", &msg), &right));
        }

        for index in start..end {
            let ri = rendered.len();
            let left = border_cell(BorderPos::LeftEdge { row: ri }, v);
            let right = border_cell(BorderPos::RightEdge { row: ri }, v);
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
            let row_text = format!("{prefix}{content}");
            let cw = visible_width(&row_text);
            let padded = if cw >= inner_width {
                fit_line(&row_text, inner_width as u16)
            } else {
                format!("{row_text}{}", " ".repeat(inner_width.saturating_sub(cw)))
            };
            rendered.push(framed_row(&left, &padded, &right));
        }

        if hidden_below > 0 {
            let ellipsis = theme.glyph("ellipsis");
            let msg = format!(
                "{ellipsis} {hidden_below} more line{} below",
                if hidden_below == 1 { "" } else { "s" }
            );
            let ri = rendered.len();
            let left = border_cell(BorderPos::LeftEdge { row: ri }, v);
            let right = border_cell(BorderPos::RightEdge { row: ri }, v);
            rendered.push(framed_row(&left, &theme.fg("model_accent", &msg), &right));
        }

        while rendered.len() < content_rows {
            let ri = rendered.len();
            let left = border_cell(BorderPos::LeftEdge { row: ri }, v);
            let right = border_cell(BorderPos::RightEdge { row: ri }, v);
            rendered.push(framed_row(&left, "", &right));
        }

        lines.append(&mut rendered);
    }

    // ---- bottom border ----
    lines.push(render_horizontal_border(false));

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
    Extension,
    ExtensionStatus,
    Tokens,
    Throughput,
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

fn format_microdollars(microdollars: u64) -> String {
    let dollars = microdollars as f64 / 1_000_000.0;
    if dollars >= 1.0 {
        format!("${dollars:.2}")
    } else {
        let formatted = format!("{dollars:.6}");
        let trimmed = formatted.trim_end_matches('0').trim_end_matches('.');
        let decimals = trimmed.split_once('.').map_or(0, |(_, value)| value.len());
        if decimals < 3 {
            format!("${dollars:.3}")
        } else {
            format!("${trimmed}")
        }
    }
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

fn activity_variants(state: &super::view::ShellState, now: Instant) -> Vec<String> {
    let session_elapsed = state.session_work_elapsed.saturating_add(
        state
            .run
            .current()
            .filter(|run| run.is_active())
            .map(|run| run.elapsed_at(now))
            .unwrap_or_default(),
    );
    let compiled_default = matches!(
        state.theme.source(),
        crate::tui::theme::ThemeSource::CompiledDefault
    );
    if let Some(run) = state.run.current().filter(|run| run.is_active()) {
        if compiled_default {
            let activity = match run.phase() {
                crate::presentation::RunPhase::AwaitingProvider { .. } => "waiting for API",
                crate::presentation::RunPhase::AwaitingApproval { .. } => "waiting",
                crate::presentation::RunPhase::Preparing { summary } if summary == "compacting" => {
                    "compacting"
                }
                crate::presentation::RunPhase::Thinking
                | crate::presentation::RunPhase::Preparing { .. }
                | crate::presentation::RunPhase::StreamingResponse
                | crate::presentation::RunPhase::PreparingToolCall
                | crate::presentation::RunPhase::RunningTool { .. }
                | crate::presentation::RunPhase::Finished(_) => return Vec::new(),
            };
            return vec![activity.to_owned()];
        }
        let elapsed = format_duration(session_elapsed);
        let activity = match run.phase() {
            crate::presentation::RunPhase::AwaitingProvider { .. } => {
                format!("waiting for API {elapsed}")
            }
            crate::presentation::RunPhase::AwaitingApproval { .. } => {
                format!("waiting {elapsed}")
            }
            crate::presentation::RunPhase::Preparing { summary } if summary == "compacting" => {
                format!("compacting {elapsed}")
            }
            crate::presentation::RunPhase::Thinking
            | crate::presentation::RunPhase::Preparing { .. }
            | crate::presentation::RunPhase::StreamingResponse
            | crate::presentation::RunPhase::PreparingToolCall
            | crate::presentation::RunPhase::RunningTool { .. } => elapsed,
            crate::presentation::RunPhase::Finished(_) => return Vec::new(),
        };
        return vec![activity];
    }
    if session_elapsed.is_zero() {
        Vec::new()
    } else if compiled_default {
        // The compiled default keeps its idle footer purely informational.
        // Named themes may opt into the decorative session stopwatch, while
        // active waiting state remains visible above through the run branch.
        Vec::new()
    } else {
        vec![format_duration(session_elapsed)]
    }
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
    if let Some((text, _)) = state
        .extension_footer
        .as_ref()
        .filter(|(text, _)| !text.trim().is_empty())
    {
        segments.push(FooterSegment::new(
            FooterKind::Extension,
            vec![text.clone()],
        ));
    }
    if let Some((text, _)) = state
        .extension_status
        .as_ref()
        .filter(|(text, _)| !text.trim().is_empty())
    {
        segments.push(FooterSegment::new(
            FooterKind::ExtensionStatus,
            vec![text.clone()],
        ));
    }

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

    let live_rate = show_turn_telemetry.then_some(()).and_then(|()| {
        state.turn_generation_started_at.and_then(|started| {
            let elapsed = now.saturating_duration_since(started);
            let tokens = state.live_generated_tokens()?;
            (elapsed >= Duration::from_millis(250) && tokens >= 2)
                .then(|| tokens as f64 / elapsed.as_secs_f64())
                .filter(|rate| rate.is_finite() && *rate > 0.0)
        })
    });
    if let Some((rate, estimated)) = live_rate.map(|rate| (rate, true)).or_else(|| {
        show_turn_telemetry
            .then(|| {
                state
                    .last_turn_tokens_per_second
                    .filter(|rate| rate.is_finite() && *rate > 0.0)
                    .map(|rate| (rate, false))
            })
            .flatten()
    }) {
        segments.push(FooterSegment::new(
            FooterKind::Throughput,
            vec![format!(
                "{}{rate:.1} tok/s",
                if estimated { "~" } else { "" }
            )],
        ));
    }

    let price_display = if active {
        state.run_price_display.unwrap_or(state.price_display)
    } else {
        state.price_display
    };
    // This is the durable session total, not the cost accumulated by the
    // current autonomous run. Session spend remains meaningful when the
    // selected model changes, so it must not depend on turn telemetry ownership.
    let cost = if let Some(cost) = state.session_cost_microdollars {
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
    // controls telemetry as semantic groups. Explicit extension contributions
    // remain visible because they are independently enabled product surfaces.
    if !layout.show_footer || layout.show_header {
        hide_footer_kind(&mut segments, FooterKind::Identity);
    }
    if !layout.show_status_line {
        for kind in [
            FooterKind::Tokens,
            FooterKind::Throughput,
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
    for kind in [
        FooterKind::CacheHit,
        FooterKind::Tokens,
        FooterKind::ExtensionStatus,
        FooterKind::Extension,
    ] {
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
    for kind in [FooterKind::Cost, FooterKind::Throughput] {
        if footer_width(&segments, gap) > available {
            hide_footer_kind(&mut segments, kind);
        }
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
        FooterKind::Identity => state.theme.fg("foreground", segment.text()),
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
        FooterKind::Activity => state.theme.model_fg(
            if active {
                state.run_model_lab
            } else {
                state.model_lab
            },
            segment.text(),
        ),
        FooterKind::Extension => {
            let role = state
                .extension_footer
                .as_ref()
                .and_then(|(_, role)| role.as_deref())
                .unwrap_or("extension.status");
            state.theme.apply_semantic_role(role, segment.text())
        }
        FooterKind::ExtensionStatus => {
            let role = state
                .extension_status
                .as_ref()
                .and_then(|(_, role)| role.as_deref())
                .unwrap_or("extension.status");
            state.theme.apply_semantic_role(role, segment.text())
        }
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

fn append_status_footer(
    lines: &mut Vec<String>,
    state: &super::view::ShellState,
    width: u16,
    now: Instant,
) {
    let layout = state.theme.layout_for_width(width);
    let has_extension = state
        .extension_footer
        .as_ref()
        .is_some_and(|(text, _)| !text.trim().is_empty())
        || state
            .extension_status
            .as_ref()
            .is_some_and(|(text, _)| !text.trim().is_empty());
    let has_identity = layout.show_footer && !layout.show_header;
    if has_identity || layout.show_status_line || has_extension {
        lines.push(render_status_footer(state, width, now));
    }
}

/// Format a token count compactly: `1.2k`, `856`, `1.0m`.
fn compact_token_count(n: u64) -> String {
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

/// Render the full composer surface: top border, content rows, bottom border,
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
        width
            .saturating_sub(2 + layout.composer_padding.saturating_mul(2))
            .max(1)
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

    // Unified composer box with perimeter shimmer on all four edges
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
    fn overflow_count() {
        assert_eq!(composer_overflow_count(5, 3), 2);
        assert_eq!(composer_overflow_count(3, 5), 0);
    }

    #[test]
    fn gradient_builds_symmetric() {
        let g = build_gradient(100, 150, 200, 5);
        assert_eq!(g.len(), 5);
        let mid = g[2].0 as u32 + g[2].1 as u32 + g[2].2 as u32;
        let edge = g[0].0 as u32 + g[0].1 as u32 + g[0].2 as u32;
        assert!(mid >= edge);
    }

    #[test]
    fn gradient_empty() {
        assert!(build_gradient(100, 100, 100, 0).is_empty());
    }

    #[test]
    fn get_gp_maps_clockwise() {
        let w = 80;
        let cr = 3;
        let perimeter = 2 * w + 2 * cr; // 160 + 6 = 166

        // Top-left: 0
        assert_eq!(get_gp(BorderPos::TopLeft, w, cr), 0);
        // Top edge: col 1 to 78
        assert_eq!(get_gp(BorderPos::TopEdge { col: 1 }, w, cr), 1);
        assert_eq!(get_gp(BorderPos::TopEdge { col: w - 2 }, w, cr), w - 2);
        // Top-right: 79
        assert_eq!(get_gp(BorderPos::TopRight, w, cr), w - 1);

        // Right edge: row 0 to cr-1 (top to bottom)
        assert_eq!(get_gp(BorderPos::RightEdge { row: 0 }, w, cr), w);
        assert_eq!(
            get_gp(BorderPos::RightEdge { row: cr - 1 }, w, cr),
            w + cr - 1
        );

        // Bottom-right: w + cr
        assert_eq!(get_gp(BorderPos::BottomRight, w, cr), w + cr);

        // Bottom edge: col 1 to w-2 (right to left)
        assert_eq!(
            get_gp(BorderPos::BottomEdge { col: w - 2 }, w, cr),
            w + cr + 1
        );
        assert_eq!(
            get_gp(BorderPos::BottomEdge { col: 1 }, w, cr),
            w + cr + w - 2
        );

        // Bottom-left: 2 * w + cr - 1
        assert_eq!(get_gp(BorderPos::BottomLeft, w, cr), 2 * w + cr - 1);

        // Left edge: row 0 to cr-1 (bottom to top, so row cr-1 is closest to bottom-left)
        assert_eq!(
            get_gp(BorderPos::LeftEdge { row: cr - 1 }, w, cr),
            2 * w + cr
        );
        assert_eq!(get_gp(BorderPos::LeftEdge { row: 0 }, w, cr), perimeter - 1);
    }

    #[test]
    fn thinking_levels_scale_monotonically() {
        let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];
        let intensities = levels
            .iter()
            .map(|level| thinking_intensity(level, false))
            .collect::<Vec<_>>();
        assert!(intensities.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(intensities.first(), Some(&0.0));
        assert_eq!(intensities.last(), Some(&1.0));
    }

    #[test]
    fn working_shimmer_velocity_is_phase_independent() {
        use crate::presentation::RunPhase;

        let phases = [
            RunPhase::Preparing {
                summary: String::new(),
            },
            RunPhase::AwaitingProvider {
                provider: String::new(),
            },
            RunPhase::Thinking,
            RunPhase::StreamingResponse,
            RunPhase::PreparingToolCall,
            RunPhase::RunningTool {
                summary: String::new(),
            },
        ];
        let speeds: Vec<_> = phases
            .iter()
            .map(|phase| phase_speed_for(Some(phase)))
            .collect();
        assert!(speeds.iter().all(|speed| *speed == speeds[0]));
        assert!(speeds[0] > 0.0);
        assert_eq!(
            phase_speed_for(Some(&RunPhase::AwaitingApproval {
                prompt: String::new(),
            })),
            0.0
        );
    }

    #[test]
    fn sine_wave_brightness_is_symmetric() {
        // Sharp cosine wave (powf 3.0): at phase 0.5 the peak is 1.0.
        let phase = 0.5;
        let raw = ((phase - 0.5) * 2.0 * std::f64::consts::PI).cos();
        let peak = (raw * 0.5 + 0.5).powf(3.0);
        assert!((peak - 1.0).abs() < 0.001);

        // At phase 0.0 and 1.0 it's near 0.0.
        let valley1_raw = ((0.0 - 0.5) * 2.0 * std::f64::consts::PI).cos();
        let valley1 = (valley1_raw * 0.5 + 0.5).powf(3.0);
        let valley2_raw = ((1.0 - 0.5) * 2.0 * std::f64::consts::PI).cos();
        let valley2 = (valley2_raw * 0.5 + 0.5).powf(3.0);
        assert!((valley1 - 0.0).abs() < 0.001);
        assert!((valley2 - 0.0).abs() < 0.001);

        // The wave is symmetric around 0.5.
        let left_raw = ((0.3 - 0.5) * 2.0 * std::f64::consts::PI).cos();
        let left = (left_raw * 0.5 + 0.5).powf(3.0);
        let right_raw = ((0.7 - 0.5) * 2.0 * std::f64::consts::PI).cos();
        let right = (right_raw * 0.5 + 0.5).powf(3.0);
        assert!((left - right).abs() < 0.001);
        // With powf 3 the values are smaller away from the peak.
        assert!(left > 0.1 && left < 1.0);
    }
}
