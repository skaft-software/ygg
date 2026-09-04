//! Shared semantic chrome for transient command and picker surfaces.
//!
//! This module intentionally owns only ordinary-surface presentation. Approval
//! panels keep their separate visibility and confirmation rules.

use std::time::Instant;

use sexy_tui_rs::visible_width;

use super::terminal_text::sanitize_ordinary_surface_cell;
use super::{fit_line, semantic_separator};
use crate::tui::theme::YggTheme;

/// Semantic colour/marker role for an ordinary lifecycle status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrdinaryStatusTone {
    Pending,
    Success,
    Muted,
    Error,
    Warning,
}

impl OrdinaryStatusTone {
    fn theme_role(self) -> &'static str {
        match self {
            Self::Pending | Self::Muted => "muted",
            Self::Success => "success",
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Success => "success",
            Self::Muted => "note",
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// Presentation copy attached to a lifecycle state.
///
/// The lifecycle selects tone independently of this wording. Callers can
/// improve copy without accidentally turning an error into an accent status.
#[derive(Clone, Debug)]
pub(crate) struct OrdinarySurfaceStatus {
    pub(crate) text: String,
    expires_at: Option<Instant>,
}

impl OrdinarySurfaceStatus {
    pub(crate) fn persistent(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            expires_at: None,
        }
    }

    pub(crate) fn transient(text: impl Into<String>, expires_at: Instant) -> Self {
        Self {
            text: text.into(),
            expires_at: Some(expires_at),
        }
    }

    fn visible_at(&self, now: Instant) -> bool {
        self.expires_at.is_none_or(|expires_at| expires_at > now)
    }
}

/// Explicit lifecycle state for an ordinary command or picker surface.
///
/// `Ready` intentionally replaces an absent status. The remaining variants
/// carry the visible copy but retain their semantic status tone in the type.
#[derive(Clone, Debug)]
pub(crate) enum OrdinarySurfaceLifecycle {
    Ready,
    Loading(OrdinarySurfaceStatus),
    Success(OrdinarySurfaceStatus),
    Empty(OrdinarySurfaceStatus),
    RecoverableError(OrdinarySurfaceStatus),
    Cancelled(OrdinarySurfaceStatus),
}

impl OrdinarySurfaceLifecycle {
    pub(crate) fn loading(text: impl Into<String>) -> Self {
        Self::Loading(OrdinarySurfaceStatus::persistent(text))
    }

    pub(crate) fn success(text: impl Into<String>, expires_at: Instant) -> Self {
        Self::Success(OrdinarySurfaceStatus::transient(text, expires_at))
    }

    pub(crate) fn empty(text: impl Into<String>) -> Self {
        Self::Empty(OrdinarySurfaceStatus::persistent(text))
    }

    pub(crate) fn recoverable_error(text: impl Into<String>, expires_at: Instant) -> Self {
        Self::RecoverableError(OrdinarySurfaceStatus::transient(text, expires_at))
    }

    pub(crate) fn cancelled(text: impl Into<String>, expires_at: Instant) -> Self {
        Self::Cancelled(OrdinarySurfaceStatus::transient(text, expires_at))
    }

    pub(crate) fn tone(&self) -> Option<OrdinaryStatusTone> {
        match self {
            Self::Ready => None,
            Self::Loading(_) => Some(OrdinaryStatusTone::Pending),
            Self::Success(_) => Some(OrdinaryStatusTone::Success),
            Self::Empty(_) => Some(OrdinaryStatusTone::Muted),
            Self::RecoverableError(_) => Some(OrdinaryStatusTone::Error),
            Self::Cancelled(_) => Some(OrdinaryStatusTone::Warning),
        }
    }

    fn status(&self) -> Option<&OrdinarySurfaceStatus> {
        match self {
            Self::Ready => None,
            Self::Loading(status)
            | Self::Success(status)
            | Self::Empty(status)
            | Self::RecoverableError(status)
            | Self::Cancelled(status) => Some(status),
        }
    }

    fn fallback_copy(&self) -> &'static str {
        match self {
            Self::Ready => "",
            Self::Loading(_) => "loading",
            Self::Success(_) => "completed",
            Self::Empty(_) => "no matches",
            Self::RecoverableError(_) => "failed",
            Self::Cancelled(_) => "cancelled",
        }
    }

    pub(crate) fn is_loading(&self) -> bool {
        matches!(self, Self::Loading(_))
    }
}

/// Typed title, purpose, and lifecycle metadata for a transient ordinary
/// surface. Selection data remains owned by the existing panel drivers.
#[derive(Clone, Debug)]
pub(crate) struct OrdinarySurfaceMetadata {
    pub(crate) title: String,
    pub(crate) purpose: Option<String>,
    pub(crate) lifecycle: OrdinarySurfaceLifecycle,
}

impl OrdinarySurfaceMetadata {
    pub(crate) fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            purpose: None,
            lifecycle: OrdinarySurfaceLifecycle::Ready,
        }
    }

    pub(crate) fn with_purpose(title: impl Into<String>, purpose: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            purpose: Some(purpose.into()),
            lifecycle: OrdinarySurfaceLifecycle::Ready,
        }
    }
}

/// Render one explicit lifecycle status after terminal sanitization and before
/// trusted theme styling. A marker plus the type-derived state word and optional
/// detail keeps the status distinguishable in no-colour terminals.
pub(crate) fn render_ordinary_status(
    theme: &YggTheme,
    lifecycle: &OrdinarySurfaceLifecycle,
    now: Instant,
) -> Option<String> {
    let tone = lifecycle.tone()?;
    let status = lifecycle.status()?;
    if !status.visible_at(now) {
        return None;
    }
    let detail = sanitize_ordinary_surface_cell(&status.text, theme.unicode());
    let state_word = lifecycle.fallback_copy();
    let text = if detail.trim().is_empty() {
        state_word.to_owned()
    } else {
        format!("{state_word}{}{detail}", semantic_separator(theme))
    };
    let marker = theme.glyph(tone.glyph());
    Some(theme.fg(tone.theme_role(), &format!("{marker} {text}")))
}

/// Width-fitting class for a complete action-footer segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FooterPriority {
    /// The one action that remains and may be clipped only as a last resort.
    Primary,
    /// A complete optional action. Lower ranks disappear first; equal ranks
    /// disappear from the right so visual order remains deterministic.
    Optional { drop_rank: u8 },
}

/// A footer unit that never leaves a partial optional action behind.
///
/// `variants` also serve the existing calm status footer: it can choose a
/// shorter complete representation before deciding whether the segment hides.
#[derive(Clone, Debug)]
pub(crate) struct FooterSegment {
    variants: Vec<String>,
    variant: usize,
    visible: bool,
    priority: FooterPriority,
}

impl FooterSegment {
    pub(crate) fn variants(variants: Vec<String>) -> Self {
        Self {
            variants,
            variant: 0,
            visible: true,
            priority: FooterPriority::Primary,
        }
    }

    pub(crate) fn primary(text: impl Into<String>) -> Self {
        Self {
            variants: vec![text.into()],
            variant: 0,
            visible: true,
            priority: FooterPriority::Primary,
        }
    }

    pub(crate) fn optional(text: impl Into<String>, drop_rank: u8) -> Self {
        Self {
            variants: vec![text.into()],
            variant: 0,
            visible: true,
            priority: FooterPriority::Optional { drop_rank },
        }
    }

    pub(crate) fn text(&self) -> &str {
        &self.variants[self.variant.min(self.variants.len().saturating_sub(1))]
    }

    pub(crate) fn compact_once(&mut self) {
        if self.variant + 1 < self.variants.len() {
            self.variant += 1;
        }
    }

    pub(crate) fn hide(&mut self) {
        self.visible = false;
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visible
    }

    fn drop_rank(&self) -> Option<u8> {
        match self.priority {
            FooterPriority::Primary => None,
            FooterPriority::Optional { drop_rank } => Some(drop_rank),
        }
    }

    fn is_primary(&self) -> bool {
        self.priority == FooterPriority::Primary
    }
}

pub(crate) fn footer_width(segments: &[FooterSegment], gap: usize) -> usize {
    let visible = segments.iter().filter(|segment| segment.is_visible());
    let count = visible.clone().count();
    visible
        .map(|segment| visible_width(segment.text()))
        .sum::<usize>()
        + count.saturating_sub(1) * gap
}

fn footer_text(prefix: &str, separator: &str, segments: &[FooterSegment]) -> String {
    let body = segments
        .iter()
        .filter(|segment| segment.is_visible())
        .map(FooterSegment::text)
        .collect::<Vec<_>>()
        .join(separator);
    format!("{prefix}{body}")
}

/// Fit priority-ordered action footer segments into one terminal line.
///
/// Optional units are removed whole, first by their explicit low-priority rank
/// and then from the right when ranks tie. Once no optional unit can fit, only
/// the primary action reaches `fit_line`, so a narrow terminal never exposes a
/// clipped `select` or `cancel` hint.
pub(crate) fn fit_prioritized_footer(
    prefix: &str,
    separator: &str,
    segments: &mut [FooterSegment],
    width: u16,
) -> String {
    while visible_width(&footer_text(prefix, separator, segments)) > usize::from(width) {
        let drop_index = segments
            .iter()
            .enumerate()
            .filter(|(_, segment)| segment.is_visible())
            .filter_map(|(index, segment)| segment.drop_rank().map(|rank| (index, rank)))
            .min_by_key(|(index, rank)| (*rank, std::cmp::Reverse(*index)))
            .map(|(index, _)| index);
        let Some(drop_index) = drop_index else {
            break;
        };
        segments[drop_index].hide();
    }

    let rendered = footer_text(prefix, separator, segments);
    if visible_width(&rendered) <= usize::from(width) {
        return rendered;
    }

    // The callers construct exactly one primary segment. If a future caller
    // accidentally has more than one, keep the leftmost highest-priority one
    // rather than clipping a concatenated set of supposedly atomic hints.
    let primary = segments
        .iter()
        .find(|segment| segment.is_visible() && segment.is_primary())
        .or_else(|| segments.iter().find(|segment| segment.is_visible()));
    primary.map_or_else(String::new, |segment| {
        fit_line(&format!("{prefix}{}", segment.text()), width)
    })
}

/// Compose a capability-aware ordinary metadata phrase.
pub(crate) fn join_ordinary_metadata(theme: &YggTheme, parts: &[&str]) -> String {
    parts
        .iter()
        .copied()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(semantic_separator(theme))
}
