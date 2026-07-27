use sexy_tui_rs::visible_width;

use crate::tui::theme::{
    ThemeSurfaceAlign, ThemeSurfaceChrome, ThemeSurfaceHeading, ThemeSurfaceWidth, YggTheme,
};

use super::transcript_selection::block_copy_text;
use super::{
    collapsed_reasoning_lines, transcript_transition_rows, SurfaceGeometry, TranscriptBlock,
};

#[derive(Clone, Copy, Debug)]
pub(super) struct SurfacePlan<'a> {
    pub(super) kind: &'static str,
    pub(super) chrome: ThemeSurfaceChrome,
    pub(super) heading: ThemeSurfaceHeading,
    pub(super) label: Option<&'a str>,
    pub(super) padding: u16,
    pub(super) frame_left: u16,
    pub(super) frame_width: u16,
    pub(super) geometry: SurfaceGeometry,
}

fn transcript_surface_kind(block: &TranscriptBlock) -> &'static str {
    match block {
        TranscriptBlock::User { .. } => "user",
        TranscriptBlock::Assistant(_) => "assistant",
        TranscriptBlock::Reasoning(_) => "reasoning",
        TranscriptBlock::Tool(_) => "tool",
        TranscriptBlock::Shell(_) => "shell",
        TranscriptBlock::Outcome(_) => "outcome",
        TranscriptBlock::Notice(_) => "notice",
        TranscriptBlock::Compaction(_) => "compaction",
    }
}

pub(super) fn surface_roles(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "user" => ("surface.user", "surface.user.border", "surface.user.label"),
        "assistant" => (
            "surface.assistant",
            "surface.assistant.border",
            "surface.assistant.label",
        ),
        "reasoning" => (
            "surface.reasoning",
            "surface.reasoning.border",
            "surface.reasoning.label",
        ),
        "tool" => ("surface.tool", "surface.tool.border", "surface.tool.label"),
        "shell" => (
            "surface.shell",
            "surface.shell.border",
            "surface.shell.label",
        ),
        "outcome" => (
            "surface.outcome",
            "surface.outcome.border",
            "surface.outcome.label",
        ),
        "notice" => (
            "surface.notice",
            "surface.notice.border",
            "surface.notice.label",
        ),
        "compaction" => (
            "surface.compaction",
            "surface.compaction.border",
            "surface.compaction.label",
        ),
        _ => ("text", "border", "muted"),
    }
}

fn natural_surface_width(block: &TranscriptBlock, theme: &YggTheme) -> u16 {
    let copy = match block {
        TranscriptBlock::Reasoning(reasoning) if !reasoning.reasoning_expanded => {
            collapsed_reasoning_lines(theme, reasoning, false).join("\n")
        }
        TranscriptBlock::Compaction(compaction) if !compaction.expanded => {
            format!("{} · (ctrl+o to view)", compaction.label)
        }
        _ => block_copy_text(block),
    };
    let natural = copy.lines().map(visible_width).max().unwrap_or(1);
    let inner_prefix = match block {
        TranscriptBlock::User { .. } => 2,
        TranscriptBlock::Reasoning(_) => visible_width(theme.glyph("reasoning")).saturating_add(1),
        TranscriptBlock::Tool(_) => 8,
        TranscriptBlock::Notice(_) | TranscriptBlock::Compaction(_) => {
            visible_width(theme.glyph("note")).saturating_add(1)
        }
        TranscriptBlock::Shell(_) => visible_width(theme.glyph("shell")).saturating_add(1),
        TranscriptBlock::Assistant(_) | TranscriptBlock::Outcome(_) => 0,
    };
    u16::try_from(natural.saturating_add(inner_prefix)).unwrap_or(u16::MAX)
}

pub(super) fn compile_surface_plan<'a>(
    previous: Option<&TranscriptBlock>,
    block: &TranscriptBlock,
    theme: &'a YggTheme,
    outer_width: u16,
) -> SurfacePlan<'a> {
    let layout = theme.layout_for_width(outer_width);
    let kind = transcript_surface_kind(block);
    let resolved = theme.surface_for_width(kind, outer_width);
    let inset = if matches!(block, TranscriptBlock::User { .. }) {
        0
    } else {
        layout.transcript_inset.min(outer_width.saturating_sub(1))
    };
    let available = outer_width.saturating_sub(inset).max(1);
    let mut chrome = resolved.chrome;
    let mut heading = if resolved.label.is_some() {
        resolved.heading
    } else {
        ThemeSurfaceHeading::None
    };
    let mut padding = resolved.padding;

    let overhead_for = |chrome: ThemeSurfaceChrome, padding: u16| -> u16 {
        let horizontal_padding = padding.saturating_mul(2);
        match chrome {
            ThemeSurfaceChrome::Plain | ThemeSurfaceChrome::Band | ThemeSurfaceChrome::Rule => {
                horizontal_padding
            }
            ThemeSurfaceChrome::Rail => u16::try_from(visible_width(theme.glyph("rail")))
                .unwrap_or(u16::MAX)
                .saturating_add(1)
                .saturating_add(horizontal_padding),
            ThemeSurfaceChrome::Card => 2u16.saturating_add(horizontal_padding),
        }
    };
    let mut overhead = overhead_for(chrome, padding);
    if available <= overhead.saturating_add(3) {
        chrome = ThemeSurfaceChrome::Plain;
        heading = ThemeSurfaceHeading::None;
        padding = 0;
        overhead = 0;
    }

    let frame_limit = resolved
        .max_width
        .unwrap_or(available)
        .min(available)
        .max(1);
    let frame_width = match resolved.width {
        ThemeSurfaceWidth::Full => frame_limit,
        ThemeSurfaceWidth::Content => {
            let requested = natural_surface_width(block, theme).saturating_add(overhead);
            requested.max(frame_limit.min(12)).min(frame_limit)
        }
    };
    if frame_width <= overhead {
        chrome = ThemeSurfaceChrome::Plain;
        heading = ThemeSurfaceHeading::None;
        padding = 0;
        overhead = 0;
    }
    let frame_offset = match resolved.align {
        ThemeSurfaceAlign::Left => 0,
        ThemeSurfaceAlign::Center => available.saturating_sub(frame_width) / 2,
        ThemeSurfaceAlign::Right => available.saturating_sub(frame_width),
    };
    let frame_left = inset.saturating_add(frame_offset);
    let chrome_left = match chrome {
        ThemeSurfaceChrome::Rail => u16::try_from(visible_width(theme.glyph("rail")))
            .unwrap_or(u16::MAX)
            .saturating_add(1),
        ThemeSurfaceChrome::Card => 1,
        ThemeSurfaceChrome::Plain | ThemeSurfaceChrome::Band | ThemeSurfaceChrome::Rule => 0,
    };
    let content_left = frame_left
        .saturating_add(chrome_left)
        .saturating_add(padding);
    let content_width = frame_width.saturating_sub(overhead).max(1);
    let is_user_card = kind == "user" && chrome == ThemeSurfaceChrome::Card;
    let leading_rows = usize::from(
        chrome == ThemeSurfaceChrome::Card
            || chrome == ThemeSurfaceChrome::Rule
            || heading != ThemeSurfaceHeading::None,
    ) + usize::from(is_user_card);
    let trailing_rows = usize::from(chrome == ThemeSurfaceChrome::Card) + usize::from(is_user_card);
    SurfacePlan {
        kind,
        chrome,
        heading,
        label: resolved.label,
        padding,
        frame_left,
        frame_width,
        geometry: SurfaceGeometry {
            transition_rows: transcript_transition_rows(previous, layout.density),
            leading_rows,
            trailing_rows,
            content_left,
            content_width,
        },
    }
}
