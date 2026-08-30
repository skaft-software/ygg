//! Shared responsive geometry for Ygg-owned terminal surfaces.

use crate::tui::theme::YggTheme;

/// Above this width, picker metadata can return to stable side-by-side columns.
/// Regular terminals keep a stacked title/detail rhythm instead of squeezing
/// either column.
const WIDE_PICKER_COLUMNS: u16 = 112;

/// Prompt and event markers own one cell plus one separating space. The
/// presentation inset begins outside that gutter; renderers must not count the
/// same two cells again.
pub(crate) const PRIMARY_TEXT_GUTTER: u16 = 2;

/// Existing themes express one cell of composer padding as the baseline, not
/// as an additional application-level margin.
const BASE_COMPOSER_PADDING: u16 = 1;

/// Queued steering is a pending-state hint, not a second transcript.
pub(crate) const MAX_STEERING_PREVIEW_ROWS: usize = 2;

/// Approval consequences remain bounded while leaving room for the selected
/// action at every usable terminal height.
pub(crate) const MAX_APPROVAL_DETAIL_ROWS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickerLayout {
    Compact,
    Stacked,
    Columns,
}

/// One width decision shared by transcript surfaces, composer, footer, and
/// pickers. Semantic theme options still decide whether optional chrome is
/// present; this plan only keeps their geometry and responsive breakpoints in
/// agreement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PresentationLayout {
    pub(crate) inset: u16,
    pub(crate) content_width: u16,
    pub(crate) picker: PickerLayout,
    pub(crate) footer_gap: usize,
}

impl PresentationLayout {
    pub(crate) fn new(theme: &YggTheme, width: u16) -> Self {
        let resolved = theme.layout_for_width(width);
        // Legacy theme tokens describe the primary text column, including the
        // marker gutter. Normalize those baselines before applying one shared
        // *outer* inset. Otherwise the marker gutter is counted twice and all
        // transcript, composer, and picker surfaces shift inward by one level.
        let requested_inset = resolved
            .transcript_inset
            .saturating_sub(PRIMARY_TEXT_GUTTER)
            .max(
                resolved
                    .composer_padding
                    .saturating_sub(BASE_COMPOSER_PADDING),
            );
        let inset = if width >= 5 {
            requested_inset.min(width.saturating_sub(1) / 2)
        } else {
            0
        };
        let content_width = width.saturating_sub(inset.saturating_mul(2)).max(1);
        let picker = if resolved.narrow {
            PickerLayout::Compact
        } else if width >= WIDE_PICKER_COLUMNS {
            PickerLayout::Columns
        } else {
            PickerLayout::Stacked
        };

        Self {
            inset,
            content_width,
            picker,
            footer_gap: if resolved.narrow { 2 } else { 3 },
        }
    }
}

/// Internal composer rows: one line at rest, then proportional bounded growth.
pub(crate) fn composer_content_rows(terminal_rows: u16, visual_lines: usize) -> usize {
    let term = terminal_rows.max(3) as usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_grid_is_symmetric_and_uses_one_responsive_plan() {
        let theme = crate::tui::theme::test_theme();
        let ordinary = PresentationLayout::new(&theme, 80);
        assert_eq!(ordinary.inset, 0);
        assert_eq!(ordinary.content_width, 80);
        assert_eq!(ordinary.picker, PickerLayout::Stacked);

        let narrow = PresentationLayout::new(&theme, 40);
        assert_eq!(narrow.inset, 0);
        assert_eq!(narrow.content_width, 40);
        assert_eq!(narrow.picker, PickerLayout::Compact);

        let wide = PresentationLayout::new(&theme, 120);
        assert_eq!(wide.inset, 0);
        assert_eq!(wide.content_width, 120);
        assert_eq!(wide.picker, PickerLayout::Columns);
    }
}
