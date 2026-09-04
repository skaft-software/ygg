#![forbid(unsafe_code)]

//! sexy-tui-rs — terminal-correct retained rendering core
//!
//! Differential terminal rendering, synchronized output, semantic rich-text
//! parsing and rendering, input sanitization, width measurement, theming, and
//! terminal capability detection.

pub mod capabilities;
pub mod glyphs;
pub mod images;
pub mod rich_text;
pub mod sanitize;
mod scrollback;
pub mod style;
pub mod terminal;
pub mod terminal_colors;
pub mod text_editor;
pub mod theme;
pub mod tui;
pub mod utils;
pub mod width;

pub use capabilities::{
    CapabilityOverrides, CapabilityProbe, CellPixelSize, ColorDepth, SupportLevel,
    TerminalCapabilities, TerminalSize, MAX_CELL_PIXEL_DIMENSION,
};
pub use glyphs::GlyphSet;
pub use images::{
    cell_rows_for_pixels, parse_terminal_image_reply, ImageAction, ImageCapabilities,
    ImageCapabilityOverrides, ImageCapabilityQuery, ImageDimensions, ImageError,
    ImageFallbackReason, ImageFilename, ImageFormat, ImageId, ImageLayout, ImageLimits,
    ImagePlanner, ImageProtocol, ImageProtocolEncoder, ImageRegistry, ImageRenderPlan,
    ImageReservation, ImageTerminalCommand, ImageViewport, TerminalImage, TerminalImageReply,
    HARD_MAX_CONTAINER_ITEMS, HARD_MAX_ENCODED_OUTPUT_BYTES, HARD_MAX_FILENAME_BYTES,
    HARD_MAX_HEADER_BYTES, HARD_MAX_IMAGE_DIMENSION, HARD_MAX_IMAGE_PAYLOAD_BYTES,
    HARD_MAX_IMAGE_PIXELS, HARD_MAX_LIVE_IMAGES, HARD_MAX_PROTOCOL_CHUNKS,
    HARD_MAX_PROTOCOL_CHUNK_BYTES, HARD_MAX_QUERY_TIMEOUT, HARD_MAX_TERMINAL_REPLY_BYTES,
    MAX_IMAGE_CELL_COLUMNS, MAX_RESERVED_IMAGE_ROWS,
};
pub use rich_text::diff::{DiffLine, DiffLineKind, DiffRenderOptions, UnifiedDiff};
pub use rich_text::markdown::parse as parse_markdown;
pub use rich_text::render::{
    CodeOverflow, RenderOptions, RenderedDocument, RenderedLine, RichRenderer, SyntaxCacheStats,
    UnorderedListMarker,
};
pub use rich_text::stream::{
    StreamingLineUpdate, StreamingMarkdown, StreamingRenderCache, StreamingStats,
    MAX_UNSTABLE_PARSE_BYTES,
};
pub use rich_text::{
    Block, CodeBlock, DetailBlock, Document, Inline, List, ListItem, ListKind, StatusKind,
    StyledSpan, Table, TableAlignment, TableCell,
};
pub use sanitize::{
    safe_hyperlink, sanitize_line, sanitize_text, ControlPictures, SafeUrl, SanitizeOptions,
};
pub use style::{BlockRole, BlockStyle, Color, TextAttributes, TextRole, TextStyle};
pub use terminal::{
    is_apple_terminal_session, key_text, normalize_apple_terminal_input,
    parse_keyboard_protocol_negotiation_sequence, KeyboardProtocolNegotiationSequence,
    ProcessTerminal, Terminal, TerminalInput,
};
pub use terminal_colors::{
    is_osc11_background_color_response, parse_osc11_background_color,
    parse_terminal_color_scheme_report, RgbColor, TerminalColorScheme,
};
pub use text_editor::{
    TextEditAction, TextEditor, TextEditorLayout, TextEditorProjection, TextEditorVisualLine,
};
pub use theme::capability::CapabilityTier;
pub use theme::Theme;
pub use tui::{
    CommitCursor, CommitPosition, Component, FrameUpdate, InputListener, PinnedFrame,
    CURSOR_MARKER, TUI,
};
pub use utils::{
    apply_background_to_line, extract_ansi_code, extract_segments, is_punctuation_char,
    is_whitespace_char, normalize_terminal_output, slice_by_column, slice_with_width,
    strip_terminal_sequences, terminal_tokens, truncate_to_width, truncate_to_width_padded,
    visible_width, wrap_text_with_ansi, AnsiCode, ColumnSlice, ExtractedSegments, TerminalToken,
};
pub use width::{display_width, AmbiguousWidth, WidthPolicy};
