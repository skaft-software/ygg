//! Bounded semantic frames for components rendered outside the terminal owner.
//!
//! Remote renderers may use printable text, SGR styling, and HTTP(S) OSC 8
//! links. This module converts that narrow subset into typed values and rejects
//! every other terminal control. Image data and terminal ownership deliberately
//! stay outside this contract: an image row contains only an opaque, validated
//! identifier and cell geometry.

use std::fmt;
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use unicode_segmentation::UnicodeSegmentation;

use crate::sanitize::SafeUrl;
use crate::width::WidthPolicy;

/// Maximum encoded size of one remote frame (256 KiB).
pub const MAX_REMOTE_FRAME_BYTES: usize = 256 * 1024;
/// Maximum physical rows in one remote frame.
pub const MAX_REMOTE_ROWS: usize = 4_096;
/// Maximum spans in a single text row.
pub const MAX_REMOTE_SPANS_PER_ROW: usize = 4_096;
/// Maximum spans across a complete frame.
pub const MAX_REMOTE_SPANS: usize = 16_384;
/// Maximum UTF-8 bytes in one span's text.
pub const MAX_REMOTE_SPAN_TEXT_BYTES: usize = 64 * 1024;
/// Maximum UTF-8 bytes in one safe link target.
pub const MAX_REMOTE_LINK_BYTES: usize = 8 * 1024;
/// Maximum UTF-8 bytes in a component or image identifier.
pub const MAX_REMOTE_ID_BYTES: usize = 256;
/// Maximum viewport width accepted from a remote renderer.
pub const MAX_REMOTE_WIDTH: u16 = 4_096;
/// Maximum images in one frame.
pub const MAX_REMOTE_IMAGES: usize = 256;
/// Largest generation/revision exactly representable by supported JSON peers.
pub const MAX_REMOTE_WIRE_INTEGER: u64 = (1_u64 << 53) - 1;

const MAX_SGR_BYTES: usize = 128;
const MAX_SGR_PARAMETERS: usize = 32;
const MAX_OSC8_PARAMETER_BYTES: usize = 128;

/// Stable classification for a remote-frame validation failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteFrameErrorKind {
    FrameTooLarge,
    LimitExceeded,
    InvalidIdentifier,
    InvalidText,
    InvalidStyle,
    UnsafeLink,
    UnsupportedTerminalSequence,
    InvalidGeometry,
    IdentityMismatch,
    GenerationMismatch,
    RevisionMismatch,
    WidthMismatch,
    NonMonotonicRevision,
}

/// Error returned by the remote row codec or frame validator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFrameError {
    kind: RemoteFrameErrorKind,
    message: String,
}

impl RemoteFrameError {
    fn new(kind: RemoteFrameErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn context(self, context: impl fmt::Display) -> Self {
        Self {
            kind: self.kind,
            message: format!("{context}: {}", self.message),
        }
    }

    /// Stable error category suitable for host policy decisions.
    pub const fn kind(&self) -> RemoteFrameErrorKind {
        self.kind
    }
}

impl fmt::Display for RemoteFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for RemoteFrameError {}

fn validate_opaque_id(
    value: &str,
    kind: &'static str,
    limit: usize,
) -> Result<(), RemoteFrameError> {
    if value.is_empty() || value.len() > limit {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidIdentifier,
            format!("remote {kind} must be 1..={limit} UTF-8 bytes"),
        ));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
    }) {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidIdentifier,
            format!("remote {kind} contains unsupported characters"),
        ));
    }
    Ok(())
}

macro_rules! opaque_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Validate and construct the opaque identifier.
            pub fn parse(value: impl Into<String>) -> Result<Self, RemoteFrameError> {
                let value = value.into();
                validate_opaque_id(&value, $description, MAX_REMOTE_ID_BYTES)?;
                Ok(Self(value))
            }

            /// Borrow the uninterpreted identifier value.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = RemoteFrameError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = RemoteFrameError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl TryFrom<&str> for $name {
            type Error = RemoteFrameError;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(de::Error::custom)
            }
        }
    };
}

opaque_id!(RemoteComponentId, "component identifier");
opaque_id!(
    RemoteImageId,
    "image identifier supplied by the host artifact boundary"
);

/// A validated HTTP(S) target safe to carry as semantic OSC 8 data.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RemoteLink(String);

impl RemoteLink {
    /// Parse an absolute HTTP(S) URL without credentials or terminal controls.
    pub fn parse(target: impl AsRef<str>) -> Result<Self, RemoteFrameError> {
        let target = target.as_ref();
        if target.is_empty() || target.len() > MAX_REMOTE_LINK_BYTES {
            return Err(unsafe_link(format!(
                "remote link must be 1..={MAX_REMOTE_LINK_BYTES} UTF-8 bytes"
            )));
        }
        if target != target.trim()
            || target.chars().any(|character| {
                character.is_control()
                    || character.is_whitespace()
                    || is_bidi_control(character)
                    || matches!(character, '\\' | '"' | '<' | '>' | '`')
            })
        {
            return Err(unsafe_link(
                "remote link contains whitespace, controls, or unsafe delimiters",
            ));
        }

        let Some(colon) = target.find(':') else {
            return Err(unsafe_link("remote link is not an absolute URL"));
        };
        let scheme = &target[..colon];
        if !matches_ignore_ascii_case(scheme, &["http", "https"]) {
            return Err(unsafe_link("remote link must use HTTP or HTTPS"));
        }
        let remainder = &target[colon + 1..];
        let Some(authority_and_path) = remainder.strip_prefix("//") else {
            return Err(unsafe_link("remote HTTP(S) link requires an authority"));
        };
        let authority_end = authority_and_path
            .find(['/', '?', '#'])
            .unwrap_or(authority_and_path.len());
        let authority = &authority_and_path[..authority_end];
        validate_url_authority(authority)?;

        let safe = SafeUrl::parse(target)
            .ok_or_else(|| unsafe_link("remote link failed terminal URL validation"))?;
        if safe.as_str().len() > MAX_REMOTE_LINK_BYTES {
            return Err(unsafe_link(format!(
                "encoded remote link exceeds {MAX_REMOTE_LINK_BYTES} bytes"
            )));
        }
        Ok(Self(safe.as_str().to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), RemoteFrameError> {
        let reparsed = Self::parse(&self.0)?;
        if reparsed.0 != self.0 {
            return Err(unsafe_link("remote link is not canonically encoded"));
        }
        Ok(())
    }
}

impl fmt::Display for RemoteLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for RemoteLink {
    type Err = RemoteFrameError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for RemoteLink {
    type Error = RemoteFrameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for RemoteLink {
    type Error = RemoteFrameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Serialize for RemoteLink {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RemoteLink {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let target = String::deserialize(deserializer)?;
        Self::parse(target).map_err(de::Error::custom)
    }
}

fn unsafe_link(message: impl Into<String>) -> RemoteFrameError {
    RemoteFrameError::new(RemoteFrameErrorKind::UnsafeLink, message)
}

fn validate_url_authority(authority: &str) -> Result<(), RemoteFrameError> {
    if authority.is_empty() {
        return Err(unsafe_link("remote link requires a non-empty host"));
    }
    if authority.contains('@') {
        return Err(unsafe_link(
            "remote link must not contain embedded credentials",
        ));
    }

    if let Some(bracketed) = authority.strip_prefix('[') {
        let Some(close) = bracketed.find(']') else {
            return Err(unsafe_link("remote link has an invalid IPv6 authority"));
        };
        if close == 0 {
            return Err(unsafe_link("remote link requires a non-empty host"));
        }
        let suffix = &bracketed[close + 1..];
        if !suffix.is_empty() {
            let Some(port) = suffix.strip_prefix(':') else {
                return Err(unsafe_link("remote link has an invalid IPv6 authority"));
            };
            validate_url_port(port)?;
        }
        return Ok(());
    }

    if authority.matches(':').count() > 1 {
        return Err(unsafe_link(
            "remote IPv6 link authority must use square brackets",
        ));
    }
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, None), |(host, port)| (host, Some(port)));
    if host.is_empty() {
        return Err(unsafe_link("remote link requires a non-empty host"));
    }
    if let Some(port) = port {
        validate_url_port(port)?;
    }
    Ok(())
}

fn validate_url_port(port: &str) -> Result<(), RemoteFrameError> {
    if port.is_empty()
        || !port.bytes().all(|byte| byte.is_ascii_digit())
        || port.parse::<u16>().is_err()
    {
        return Err(unsafe_link("remote link contains an invalid port"));
    }
    Ok(())
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

/// A color carried by a remote semantic span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteColor {
    #[default]
    Default,
    /// Base ANSI color; only values 0..=15 are valid.
    Ansi16(u8),
    Indexed(u8),
    Rgb {
        red: u8,
        green: u8,
        blue: u8,
    },
}

impl RemoteColor {
    fn validate(self) -> Result<(), RemoteFrameError> {
        if let Self::Ansi16(index) = self {
            if index >= 16 {
                return Err(RemoteFrameError::new(
                    RemoteFrameErrorKind::InvalidStyle,
                    format!("remote ANSI-16 color index {index} is outside 0..=15"),
                ));
            }
        }
        Ok(())
    }
}

/// Supported non-color SGR attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteTextAttributes {
    #[serde(default)]
    pub bold: bool,
    #[serde(default)]
    pub dim: bool,
    #[serde(default)]
    pub italic: bool,
    #[serde(default)]
    pub underline: bool,
    #[serde(default)]
    pub strikethrough: bool,
    #[serde(default)]
    pub inverse: bool,
}

/// Complete style for one remote span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteStyle {
    #[serde(default)]
    pub fg: RemoteColor,
    #[serde(default)]
    pub bg: RemoteColor,
    #[serde(default)]
    pub attributes: RemoteTextAttributes,
}

impl RemoteStyle {
    pub const fn plain() -> Self {
        Self {
            fg: RemoteColor::Default,
            bg: RemoteColor::Default,
            attributes: RemoteTextAttributes {
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                strikethrough: false,
                inverse: false,
            },
        }
    }

    fn validate(self) -> Result<(), RemoteFrameError> {
        self.fg.validate()?;
        self.bg.validate()
    }
}

/// Printable text with typed style and an optional validated HTTP(S) link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteSpan {
    pub text: String,
    #[serde(default)]
    pub style: RemoteStyle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_link: Option<RemoteLink>,
}

impl RemoteSpan {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: RemoteStyle::plain(),
            safe_link: None,
        }
    }
}

/// Opaque host-resolved image placement. No bytes or terminal escapes appear here.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteImagePlacement {
    pub image_id: RemoteImageId,
    /// Zero-based start column in terminal cells.
    pub column: u16,
    /// Reserved width in terminal cells.
    pub width: u16,
    /// Reserved height in physical rows, including the placement row.
    pub height: u16,
}

/// One physical remote row: semantic spans or an opaque image placement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteRow {
    Spans { spans: Vec<RemoteSpan> },
    ImagePlacement { placement: RemoteImagePlacement },
}

impl RemoteRow {
    pub fn spans(spans: Vec<RemoteSpan>) -> Self {
        Self::Spans { spans }
    }

    pub fn plain(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::Spans {
            spans: if text.is_empty() {
                Vec::new()
            } else {
                vec![RemoteSpan::plain(text)]
            },
        }
    }

    pub fn image(placement: RemoteImagePlacement) -> Self {
        Self::ImagePlacement { placement }
    }

    pub fn as_spans(&self) -> Option<&[RemoteSpan]> {
        match self {
            Self::Spans { spans } => Some(spans),
            Self::ImagePlacement { .. } => None,
        }
    }

    fn is_empty_text(&self) -> bool {
        matches!(self, Self::Spans { spans } if spans.is_empty())
    }
}

/// Optional hardware cursor requested within a validated text row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteCursor {
    pub row: u16,
    pub column: u16,
}

/// Complete semantic frame returned by one remote component generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFrame {
    pub component_id: RemoteComponentId,
    pub generation: u64,
    pub revision: u64,
    pub width: u16,
    #[serde(default)]
    pub rows: Vec<RemoteRow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<RemoteCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired_height: Option<u16>,
}

impl RemoteFrame {
    /// Parse Pi-rendered strings into a semantic frame and validate its shape.
    pub fn from_pi_rows<S: AsRef<str>>(
        component_id: RemoteComponentId,
        generation: u64,
        revision: u64,
        width: u16,
        rows: &[S],
    ) -> Result<Self, RemoteFrameError> {
        let frame = Self {
            component_id,
            generation,
            revision,
            width,
            rows: parse_pi_rendered_rows(rows)?,
            cursor: None,
            desired_height: None,
        };
        frame.validate()?;
        Ok(frame)
    }

    /// Validate semantic bounds, printable text, cell geometry, and identifiers.
    pub fn validate(&self) -> Result<(), RemoteFrameError> {
        self.validate_with_optional_encoded_size(None)
    }

    /// Validate the frame and the exact byte count measured by its wire decoder.
    pub fn validate_with_encoded_size(&self, encoded_bytes: usize) -> Result<(), RemoteFrameError> {
        self.validate_with_optional_encoded_size(Some(encoded_bytes))
    }

    /// Conservative encoded-size estimate used when no wire byte count exists.
    pub fn estimated_encoded_bytes(&self) -> usize {
        let mut bytes = 128usize;
        bytes = bytes.saturating_add(json_string_bytes(self.component_id.as_str()));
        for row in &self.rows {
            bytes = bytes.saturating_add(32);
            match row {
                RemoteRow::Spans { spans } => {
                    for span in spans {
                        // This covers field names, the largest style form, and
                        // punctuation in a compact serde representation.
                        bytes = bytes.saturating_add(256);
                        bytes = bytes.saturating_add(json_string_bytes(&span.text));
                        if let Some(link) = &span.safe_link {
                            bytes = bytes.saturating_add(json_string_bytes(link.as_str()));
                        }
                    }
                }
                RemoteRow::ImagePlacement { placement } => {
                    bytes = bytes.saturating_add(128);
                    bytes = bytes.saturating_add(json_string_bytes(placement.image_id.as_str()));
                }
            }
        }
        if self.cursor.is_some() {
            bytes = bytes.saturating_add(64);
        }
        bytes
    }

    fn validate_with_optional_encoded_size(
        &self,
        encoded_bytes: Option<usize>,
    ) -> Result<(), RemoteFrameError> {
        if let Some(encoded_bytes) = encoded_bytes {
            if encoded_bytes > MAX_REMOTE_FRAME_BYTES {
                return Err(frame_too_large(encoded_bytes));
            }
        }
        let estimated = self.estimated_encoded_bytes();
        if estimated > MAX_REMOTE_FRAME_BYTES {
            return Err(frame_too_large(estimated));
        }

        validate_opaque_id(
            self.component_id.as_str(),
            "component identifier",
            MAX_REMOTE_ID_BYTES,
        )?;
        validate_wire_integer("generation", self.generation)?;
        validate_wire_integer("revision", self.revision)?;
        validate_remote_width(self.width)?;

        if self.rows.len() > MAX_REMOTE_ROWS {
            return Err(limit_error("rows", self.rows.len(), MAX_REMOTE_ROWS));
        }
        if self
            .desired_height
            .is_some_and(|height| usize::from(height) > MAX_REMOTE_ROWS)
        {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidGeometry,
                format!("remote desired height exceeds {MAX_REMOTE_ROWS} rows"),
            ));
        }

        let mut total_spans = 0usize;
        let mut image_count = 0usize;
        let mut occupied_by_image = vec![false; self.rows.len()];
        let mut metrics = Vec::with_capacity(self.rows.len());

        for (row_index, row) in self.rows.iter().enumerate() {
            match row {
                RemoteRow::Spans { spans } => {
                    if spans.len() > MAX_REMOTE_SPANS_PER_ROW {
                        return Err(limit_error(
                            "spans in one row",
                            spans.len(),
                            MAX_REMOTE_SPANS_PER_ROW,
                        )
                        .context(format_args!("remote row {row_index}")));
                    }
                    total_spans = total_spans.saturating_add(spans.len());
                    if total_spans > MAX_REMOTE_SPANS {
                        return Err(limit_error("spans", total_spans, MAX_REMOTE_SPANS));
                    }
                    metrics.push(Some(validate_text_row(spans, self.width, row_index)?));
                }
                RemoteRow::ImagePlacement { placement } => {
                    image_count = image_count.saturating_add(1);
                    if image_count > MAX_REMOTE_IMAGES {
                        return Err(limit_error("images", image_count, MAX_REMOTE_IMAGES));
                    }
                    validate_image_placement(
                        placement,
                        row_index,
                        self.width,
                        &self.rows,
                        &mut occupied_by_image,
                    )?;
                    metrics.push(None);
                }
            }
        }

        if let Some(cursor) = self.cursor {
            validate_cursor(cursor, self.width, &self.rows, &metrics, &occupied_by_image)?;
        }
        Ok(())
    }
}

fn frame_too_large(bytes: usize) -> RemoteFrameError {
    RemoteFrameError::new(
        RemoteFrameErrorKind::FrameTooLarge,
        format!("remote frame is {bytes} bytes; limit is {MAX_REMOTE_FRAME_BYTES}"),
    )
}

fn limit_error(field: &'static str, actual: usize, limit: usize) -> RemoteFrameError {
    RemoteFrameError::new(
        RemoteFrameErrorKind::LimitExceeded,
        format!("remote frame has {actual} {field}; limit is {limit}"),
    )
}

fn validate_wire_integer(field: &'static str, value: u64) -> Result<(), RemoteFrameError> {
    if value > MAX_REMOTE_WIRE_INTEGER {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!(
                "remote {field} {value} exceeds portable integer limit {MAX_REMOTE_WIRE_INTEGER}"
            ),
        ));
    }
    Ok(())
}

fn validate_remote_width(width: u16) -> Result<(), RemoteFrameError> {
    if width == 0 || width > MAX_REMOTE_WIDTH {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!("remote width must be 1..={MAX_REMOTE_WIDTH} cells"),
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct RowMetrics {
    width: usize,
    cell_boundaries: Vec<usize>,
}

fn validate_text_row(
    spans: &[RemoteSpan],
    frame_width: u16,
    row_index: usize,
) -> Result<RowMetrics, RemoteFrameError> {
    let mut row = String::new();
    let mut span_boundaries = Vec::with_capacity(spans.len().saturating_sub(1));

    for (span_index, span) in spans.iter().enumerate() {
        if span.text.is_empty() {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidText,
                format!("remote row {row_index} span {span_index} has empty text"),
            ));
        }
        if span.text.len() > MAX_REMOTE_SPAN_TEXT_BYTES {
            return Err(limit_error(
                "UTF-8 bytes in one span",
                span.text.len(),
                MAX_REMOTE_SPAN_TEXT_BYTES,
            )
            .context(format_args!("remote row {row_index} span {span_index}")));
        }
        validate_printable_text(&span.text).map_err(|error| {
            error.context(format_args!("remote row {row_index} span {span_index}"))
        })?;
        span.style.validate().map_err(|error| {
            error.context(format_args!("remote row {row_index} span {span_index}"))
        })?;
        if let Some(link) = &span.safe_link {
            link.validate().map_err(|error| {
                error.context(format_args!("remote row {row_index} span {span_index}"))
            })?;
        }
        row.push_str(&span.text);
        if span_index + 1 < spans.len() {
            span_boundaries.push(row.len());
        }
    }

    let grapheme_starts = row
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    for boundary in span_boundaries {
        if boundary != row.len() && grapheme_starts.binary_search(&boundary).is_err() {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidText,
                format!(
                    "remote row {row_index} has a style/link span boundary inside a Unicode grapheme"
                ),
            ));
        }
    }

    let policy = WidthPolicy::default();
    let mut column = 0usize;
    let mut cell_boundaries = vec![0];
    for grapheme in row.graphemes(true) {
        let width = policy.grapheme_width(grapheme, column);
        if width == 0 {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidText,
                format!("remote row {row_index} contains an isolated zero-cell grapheme"),
            ));
        }
        column = column.checked_add(width).ok_or_else(|| {
            RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidGeometry,
                format!("remote row {row_index} cell width overflowed"),
            )
        })?;
        if column > usize::from(frame_width) {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidGeometry,
                format!(
                    "remote row {row_index} is {column} cells wide; frame width is {frame_width}"
                ),
            ));
        }
        cell_boundaries.push(column);
    }

    Ok(RowMetrics {
        width: column,
        cell_boundaries,
    })
}

fn validate_printable_text(text: &str) -> Result<(), RemoteFrameError> {
    if text
        .chars()
        .any(|character| character.is_control() || is_bidi_control(character))
    {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidText,
            "remote text contains terminal, control, newline, tab, or bidi formatting data",
        ));
    }
    Ok(())
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn validate_image_placement(
    placement: &RemoteImagePlacement,
    row_index: usize,
    frame_width: u16,
    rows: &[RemoteRow],
    occupied: &mut [bool],
) -> Result<(), RemoteFrameError> {
    validate_opaque_id(
        placement.image_id.as_str(),
        "image identifier",
        MAX_REMOTE_ID_BYTES,
    )?;
    if placement.width == 0 || placement.height == 0 {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!("remote image at row {row_index} must reserve non-zero width and height"),
        ));
    }
    if placement.width > MAX_REMOTE_WIDTH || usize::from(placement.height) > MAX_REMOTE_ROWS {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!("remote image at row {row_index} exceeds image dimension limits"),
        ));
    }
    let end_column = placement
        .column
        .checked_add(placement.width)
        .ok_or_else(|| {
            RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidGeometry,
                format!("remote image at row {row_index} column geometry overflowed"),
            )
        })?;
    if end_column > frame_width {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!(
                "remote image at row {row_index} ends at column {end_column}; frame width is {frame_width}"
            ),
        ));
    }
    let end_row = row_index
        .checked_add(usize::from(placement.height))
        .ok_or_else(|| {
            RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidGeometry,
                format!("remote image at row {row_index} row geometry overflowed"),
            )
        })?;
    if end_row > rows.len() {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!(
                "remote image at row {row_index} reserves through row {}; frame has {} rows",
                end_row.saturating_sub(1),
                rows.len()
            ),
        ));
    }
    for image_row in row_index..end_row {
        if occupied[image_row] {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidGeometry,
                format!("remote image at row {row_index} overlaps another image"),
            ));
        }
        if image_row != row_index && !rows[image_row].is_empty_text() {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::InvalidGeometry,
                format!(
                    "remote image at row {row_index} requires reserved row {image_row} to be empty"
                ),
            ));
        }
    }
    occupied[row_index..end_row].fill(true);
    Ok(())
}

fn validate_cursor(
    cursor: RemoteCursor,
    frame_width: u16,
    rows: &[RemoteRow],
    metrics: &[Option<RowMetrics>],
    occupied_by_image: &[bool],
) -> Result<(), RemoteFrameError> {
    let row = usize::from(cursor.row);
    let column = usize::from(cursor.column);
    if row >= rows.len() {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!(
                "remote cursor row {} is outside {} frame rows",
                cursor.row,
                rows.len()
            ),
        ));
    }
    if cursor.column > frame_width {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!(
                "remote cursor column {} exceeds frame width {frame_width}",
                cursor.column
            ),
        ));
    }
    if occupied_by_image[row] {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!("remote cursor row {row} is occupied by an image"),
        ));
    }
    let Some(row_metrics) = &metrics[row] else {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!("remote cursor row {row} is not a text row"),
        ));
    };
    if column < row_metrics.width && row_metrics.cell_boundaries.binary_search(&column).is_err() {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidGeometry,
            format!(
                "remote cursor column {} splits a wide Unicode cell in row {row}",
                cursor.column
            ),
        ));
    }
    Ok(())
}

fn json_string_bytes(value: &str) -> usize {
    value.chars().fold(2usize, |bytes, character| {
        bytes.saturating_add(match character {
            '"' | '\\' => 2,
            character if character.is_control() => 6,
            character => character.len_utf8(),
        })
    })
}

/// Stateful generation fence for exact render requests and monotonic replies.
///
/// A validator is bound to one component ID and process generation. Each call
/// also supplies the revision and width from the corresponding host render
/// request. The accepted revision advances only after complete frame validation.
#[derive(Debug)]
pub struct RemoteFrameValidator {
    component_id: RemoteComponentId,
    generation: u64,
    last_revision: Option<u64>,
}

impl RemoteFrameValidator {
    pub fn new(component_id: RemoteComponentId, generation: u64) -> Result<Self, RemoteFrameError> {
        validate_opaque_id(
            component_id.as_str(),
            "component identifier",
            MAX_REMOTE_ID_BYTES,
        )?;
        validate_wire_integer("generation", generation)?;
        Ok(Self {
            component_id,
            generation,
            last_revision: None,
        })
    }

    /// Validate one exact render reply and advance the accepted revision.
    pub fn validate(
        &mut self,
        frame: &RemoteFrame,
        expected_revision: u64,
        expected_width: u16,
    ) -> Result<(), RemoteFrameError> {
        self.validate_inner(frame, expected_revision, expected_width, None)
    }

    /// As [`Self::validate`], with the exact encoded frame size from the wire.
    pub fn validate_with_encoded_size(
        &mut self,
        frame: &RemoteFrame,
        expected_revision: u64,
        expected_width: u16,
        encoded_bytes: usize,
    ) -> Result<(), RemoteFrameError> {
        self.validate_inner(
            frame,
            expected_revision,
            expected_width,
            Some(encoded_bytes),
        )
    }

    pub const fn last_revision(&self) -> Option<u64> {
        self.last_revision
    }

    fn validate_inner(
        &mut self,
        frame: &RemoteFrame,
        expected_revision: u64,
        expected_width: u16,
        encoded_bytes: Option<usize>,
    ) -> Result<(), RemoteFrameError> {
        validate_wire_integer("expected revision", expected_revision)?;
        validate_remote_width(expected_width)?;
        if frame.component_id != self.component_id {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::IdentityMismatch,
                format!(
                    "remote frame component {:?} does not match expected {:?}",
                    frame.component_id.as_str(),
                    self.component_id.as_str()
                ),
            ));
        }
        if frame.generation != self.generation {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::GenerationMismatch,
                format!(
                    "remote frame generation {} does not match expected {}",
                    frame.generation, self.generation
                ),
            ));
        }
        if frame.revision != expected_revision {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::RevisionMismatch,
                format!(
                    "remote frame revision {} does not match requested {expected_revision}",
                    frame.revision
                ),
            ));
        }
        if frame.width != expected_width {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::WidthMismatch,
                format!(
                    "remote frame width {} does not match requested {expected_width}",
                    frame.width
                ),
            ));
        }
        if self
            .last_revision
            .is_some_and(|last_revision| frame.revision <= last_revision)
        {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::NonMonotonicRevision,
                format!(
                    "remote frame revision {} is not newer than accepted revision {}",
                    frame.revision,
                    self.last_revision.unwrap_or_default()
                ),
            ));
        }

        match encoded_bytes {
            Some(encoded_bytes) => frame.validate_with_encoded_size(encoded_bytes)?,
            None => frame.validate()?,
        }
        self.last_revision = Some(frame.revision);
        Ok(())
    }
}

/// Parse one Pi-rendered row into semantic spans.
///
/// Only printable text, supported SGR, and ST-terminated OSC 8 HTTP(S) links
/// are accepted. BEL is rejected even when used as an OSC terminator.
pub fn parse_pi_rendered_row(row: &str) -> Result<RemoteRow, RemoteFrameError> {
    if row.len() > MAX_REMOTE_FRAME_BYTES {
        return Err(frame_too_large(row.len()));
    }

    let mut spans = Vec::new();
    let mut style = RemoteStyle::plain();
    let mut link: Option<RemoteLink> = None;
    let mut cursor = 0usize;
    let mut text_start = 0usize;

    while cursor < row.len() {
        if row.as_bytes()[cursor] == 0x1b {
            push_parsed_text(&mut spans, &row[text_start..cursor], style, link.as_ref())?;
            let next = row
                .as_bytes()
                .get(cursor + 1)
                .copied()
                .ok_or_else(|| unsupported_sequence(cursor, "unterminated ESC"))?;
            cursor = match next {
                b'[' => parse_sgr_sequence(row, cursor, &mut style)?,
                b']' => parse_osc_sequence(row, cursor, &mut link)?,
                b'P' => {
                    return Err(unsupported_sequence(cursor, "DCS"));
                }
                b'_' => {
                    return Err(unsupported_sequence(cursor, "APC"));
                }
                b'^' => {
                    return Err(unsupported_sequence(cursor, "PM"));
                }
                b'X' => {
                    return Err(unsupported_sequence(cursor, "SOS"));
                }
                b'\\' => {
                    return Err(unsupported_sequence(cursor, "stray ST"));
                }
                _ => {
                    return Err(unsupported_sequence(cursor, "non-SGR/OSC escape"));
                }
            };
            text_start = cursor;
            continue;
        }

        let character = row[cursor..]
            .chars()
            .next()
            .expect("cursor remains on a UTF-8 boundary");
        if character.is_control() || is_bidi_control(character) {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::UnsupportedTerminalSequence,
                format!(
                    "remote row contains forbidden control U+{:04X} at byte {cursor}",
                    character as u32
                ),
            ));
        }
        cursor += character.len_utf8();
    }

    push_parsed_text(&mut spans, &row[text_start..], style, link.as_ref())?;
    if link.is_some() {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::UnsupportedTerminalSequence,
            "remote OSC 8 link is not closed before the end of its row",
        ));
    }
    validate_text_row(&spans, MAX_REMOTE_WIDTH, 0)?;
    Ok(RemoteRow::Spans { spans })
}

/// Parse bounded Pi-rendered physical rows into semantic rows.
pub fn parse_pi_rendered_rows<S: AsRef<str>>(
    rows: &[S],
) -> Result<Vec<RemoteRow>, RemoteFrameError> {
    if rows.len() > MAX_REMOTE_ROWS {
        return Err(limit_error("rows", rows.len(), MAX_REMOTE_ROWS));
    }
    let source_bytes = rows.iter().try_fold(0usize, |bytes, row| {
        bytes
            .checked_add(row.as_ref().len())
            .and_then(|bytes| bytes.checked_add(1))
    });
    let Some(source_bytes) = source_bytes else {
        return Err(frame_too_large(usize::MAX));
    };
    if source_bytes > MAX_REMOTE_FRAME_BYTES {
        return Err(frame_too_large(source_bytes));
    }

    rows.iter()
        .enumerate()
        .map(|(row_index, row)| {
            parse_pi_rendered_row(row.as_ref())
                .map_err(|error| error.context(format_args!("remote row {row_index}")))
        })
        .collect()
}

fn push_parsed_text(
    spans: &mut Vec<RemoteSpan>,
    text: &str,
    style: RemoteStyle,
    link: Option<&RemoteLink>,
) -> Result<(), RemoteFrameError> {
    if text.is_empty() {
        return Ok(());
    }
    validate_printable_text(text)?;
    if let Some(previous) = spans.last_mut() {
        if previous.style == style && previous.safe_link.as_ref() == link {
            let combined_len = previous.text.len().saturating_add(text.len());
            if combined_len > MAX_REMOTE_SPAN_TEXT_BYTES {
                return Err(limit_error(
                    "UTF-8 bytes in one span",
                    combined_len,
                    MAX_REMOTE_SPAN_TEXT_BYTES,
                ));
            }
            previous.text.push_str(text);
            return Ok(());
        }
    }
    if spans.len() >= MAX_REMOTE_SPANS_PER_ROW {
        return Err(limit_error(
            "spans in one row",
            spans.len().saturating_add(1),
            MAX_REMOTE_SPANS_PER_ROW,
        ));
    }
    if text.len() > MAX_REMOTE_SPAN_TEXT_BYTES {
        return Err(limit_error(
            "UTF-8 bytes in one span",
            text.len(),
            MAX_REMOTE_SPAN_TEXT_BYTES,
        ));
    }
    spans.push(RemoteSpan {
        text: text.to_owned(),
        style,
        safe_link: link.cloned(),
    });
    Ok(())
}

fn parse_sgr_sequence(
    row: &str,
    start: usize,
    style: &mut RemoteStyle,
) -> Result<usize, RemoteFrameError> {
    let bytes = row.as_bytes();
    let mut final_index = start + 2;
    while final_index < bytes.len() && !(0x40..=0x7e).contains(&bytes[final_index]) {
        if !(0x20..=0x3f).contains(&bytes[final_index]) {
            return Err(unsupported_sequence(start, "malformed CSI"));
        }
        final_index += 1;
    }
    if final_index >= bytes.len() {
        return Err(unsupported_sequence(start, "unterminated CSI"));
    }
    if bytes[final_index] != b'm' {
        return Err(unsupported_sequence(start, "non-SGR CSI"));
    }
    let parameters = &row[start + 2..final_index];
    apply_sgr(parameters, style).map_err(|error| error.context(format_args!("at byte {start}")))?;
    Ok(final_index + 1)
}

fn apply_sgr(parameters: &str, style: &mut RemoteStyle) -> Result<(), RemoteFrameError> {
    if parameters.len() > MAX_SGR_BYTES
        || !parameters
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b';')
    {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidStyle,
            "remote SGR has invalid or oversized parameters",
        ));
    }
    let values = if parameters.is_empty() {
        vec![0_u16]
    } else {
        parameters
            .split(';')
            .map(|part| {
                if part.is_empty() {
                    Ok(0)
                } else {
                    part.parse::<u16>().map_err(|_| {
                        RemoteFrameError::new(
                            RemoteFrameErrorKind::InvalidStyle,
                            "remote SGR parameter exceeds its numeric range",
                        )
                    })
                }
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    if values.len() > MAX_SGR_PARAMETERS {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidStyle,
            format!("remote SGR has more than {MAX_SGR_PARAMETERS} parameters"),
        ));
    }

    let mut index = 0usize;
    while index < values.len() {
        let value = values[index];
        match value {
            0 => *style = RemoteStyle::plain(),
            1 => style.attributes.bold = true,
            2 => style.attributes.dim = true,
            3 => style.attributes.italic = true,
            4 => style.attributes.underline = true,
            7 => style.attributes.inverse = true,
            9 => style.attributes.strikethrough = true,
            21 => style.attributes.bold = false,
            22 => {
                style.attributes.bold = false;
                style.attributes.dim = false;
            }
            23 => style.attributes.italic = false,
            24 => style.attributes.underline = false,
            27 => style.attributes.inverse = false,
            29 => style.attributes.strikethrough = false,
            30..=37 => style.fg = RemoteColor::Ansi16((value - 30) as u8),
            39 => style.fg = RemoteColor::Default,
            40..=47 => style.bg = RemoteColor::Ansi16((value - 40) as u8),
            49 => style.bg = RemoteColor::Default,
            90..=97 => style.fg = RemoteColor::Ansi16((value - 90 + 8) as u8),
            100..=107 => style.bg = RemoteColor::Ansi16((value - 100 + 8) as u8),
            38 | 48 => {
                let (color, consumed) = parse_extended_color(&values[index + 1..])?;
                if value == 38 {
                    style.fg = color;
                } else {
                    style.bg = color;
                }
                index += consumed;
            }
            _ => {
                return Err(RemoteFrameError::new(
                    RemoteFrameErrorKind::InvalidStyle,
                    format!("remote SGR parameter {value} is unsupported"),
                ));
            }
        }
        index += 1;
    }
    Ok(())
}

fn parse_extended_color(values: &[u16]) -> Result<(RemoteColor, usize), RemoteFrameError> {
    match values {
        [5, value, ..] if *value <= u16::from(u8::MAX) => {
            Ok((RemoteColor::Indexed(*value as u8), 2))
        }
        [2, red, green, blue, ..]
            if [red, green, blue]
                .into_iter()
                .all(|value| *value <= u16::from(u8::MAX)) =>
        {
            Ok((
                RemoteColor::Rgb {
                    red: *red as u8,
                    green: *green as u8,
                    blue: *blue as u8,
                },
                4,
            ))
        }
        _ => Err(RemoteFrameError::new(
            RemoteFrameErrorKind::InvalidStyle,
            "remote extended SGR color must be 5;n or 2;r;g;b with byte values",
        )),
    }
}

fn parse_osc_sequence(
    row: &str,
    start: usize,
    active_link: &mut Option<RemoteLink>,
) -> Result<usize, RemoteFrameError> {
    let bytes = row.as_bytes();
    let payload_start = start + 2;
    let mut cursor = payload_start;
    let payload_end = loop {
        if cursor >= bytes.len() {
            return Err(unsupported_sequence(start, "unterminated OSC"));
        }
        match bytes[cursor] {
            0x07 => {
                return Err(unsupported_sequence(start, "BEL-terminated OSC"));
            }
            0x1b if bytes.get(cursor + 1) == Some(&b'\\') => break cursor,
            0x1b => {
                return Err(unsupported_sequence(start, "embedded ESC in OSC"));
            }
            byte if byte < 0x20 || byte == 0x7f => {
                return Err(unsupported_sequence(start, "control data in OSC"));
            }
            _ => cursor += 1,
        }
    };
    let payload = &row[payload_start..payload_end];
    parse_osc8_payload(payload, active_link)
        .map_err(|error| error.context(format_args!("at byte {start}")))?;
    Ok(payload_end + 2)
}

fn parse_osc8_payload(
    payload: &str,
    active_link: &mut Option<RemoteLink>,
) -> Result<(), RemoteFrameError> {
    let mut fields = payload.splitn(3, ';');
    let command = fields.next().unwrap_or_default();
    let parameters = fields.next();
    let target = fields.next();
    if command != "8" || parameters.is_none() || target.is_none() {
        let name = if command == "52" {
            "OSC 52"
        } else {
            "unknown OSC"
        };
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::UnsupportedTerminalSequence,
            format!("remote row contains forbidden {name}"),
        ));
    }
    let parameters = parameters.unwrap_or_default();
    let target = target.unwrap_or_default();
    validate_osc8_parameters(parameters)?;

    if target.is_empty() {
        *active_link = None;
        return Ok(());
    }
    if active_link.is_some() {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::UnsupportedTerminalSequence,
            "remote OSC 8 links must close before another link opens",
        ));
    }
    *active_link = Some(RemoteLink::parse(target)?);
    Ok(())
}

fn validate_osc8_parameters(parameters: &str) -> Result<(), RemoteFrameError> {
    if parameters.is_empty() {
        return Ok(());
    }
    if parameters.len() > MAX_OSC8_PARAMETER_BYTES {
        return Err(RemoteFrameError::new(
            RemoteFrameErrorKind::UnsupportedTerminalSequence,
            format!("remote OSC 8 parameters exceed {MAX_OSC8_PARAMETER_BYTES} bytes"),
        ));
    }
    let mut seen_id = false;
    for parameter in parameters.split(':') {
        let Some((name, value)) = parameter.split_once('=') else {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::UnsupportedTerminalSequence,
                "remote OSC 8 parameters must use key=value form",
            ));
        };
        if name != "id"
            || seen_id
            || value.is_empty()
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(RemoteFrameError::new(
                RemoteFrameErrorKind::UnsupportedTerminalSequence,
                "remote OSC 8 supports at most one bounded opaque id parameter",
            ));
        }
        seen_id = true;
    }
    Ok(())
}

fn unsupported_sequence(offset: usize, name: &'static str) -> RemoteFrameError {
    RemoteFrameError::new(
        RemoteFrameErrorKind::UnsupportedTerminalSequence,
        format!("remote row contains forbidden or malformed {name} at byte {offset}"),
    )
}
